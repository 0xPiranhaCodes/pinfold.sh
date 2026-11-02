//! Search and selection across a pane's scrollback store and screen.

use pinfold_core::scrollback::LINE_CAP;
use pinfold_core::search::{Direction, MAX_SELECTION, Position, Search, SearchStep, Selection};
use pinfold_core::vt::{AlacrittyEngine, VtEngine};
use proptest::prelude::*;

const ROWS: u16 = 4;
const COLUMNS: u16 = 20;

fn feed(engine: &mut AlacrittyEngine, bytes: &[u8]) {
    engine.feed(bytes, &mut Vec::new());
}

fn pane(bytes: &[u8]) -> AlacrittyEngine {
    let mut engine = AlacrittyEngine::new(ROWS, COLUMNS);
    feed(&mut engine, bytes);
    engine
}

fn numbers(first: usize, last: usize) -> String {
    (first..=last)
        .map(|number| format!("{number}\r\n"))
        .collect()
}

fn at(row: usize, column: u16) -> Position {
    Position { row, column }
}

/// Runs a search to its end, and returns the match and the steps it took.
fn run(engine: &AlacrittyEngine, mut search: Search) -> (Option<Selection>, usize) {
    let mut steps = 1;
    loop {
        match engine.search(&mut search) {
            SearchStep::Running => steps += 1,
            SearchStep::Found(found) => return (Some(found), steps),
            SearchStep::NotFound => return (None, steps),
        }
    }
}

fn find(
    engine: &AlacrittyEngine,
    query: &str,
    from: Position,
    direction: Direction,
) -> Option<Selection> {
    run(engine, Search::new(query, from, direction)?).0
}

fn forward(engine: &AlacrittyEngine, query: &str) -> Option<Selection> {
    find(engine, query, Position::default(), Direction::Forward)
}

fn backward(engine: &AlacrittyEngine, query: &str) -> Option<Selection> {
    find(engine, query, at(usize::MAX, 0), Direction::Backward)
}

fn copied(engine: &AlacrittyEngine, selection: Selection) -> String {
    let mut text = String::new();
    engine.selection_text(selection, &mut text);
    text
}

fn all(engine: &AlacrittyEngine) -> String {
    let selection = Selection {
        anchor: Position::default(),
        focus: at(usize::MAX, u16::MAX),
    };
    copied(engine, selection)
}

#[test]
fn search_finds_text_in_history_and_on_the_screen() {
    let engine = pane(b"cold\r\n1\r\n2\r\n3\r\n4\r\nlive");
    let cold = forward(&engine, "cold").map(|found| found.anchor);
    assert_eq!(cold, Some(at(0, 0)));
    let live = forward(&engine, "live").map(|found| found.anchor);
    assert_eq!(live, Some(at(5, 0)));
    assert_eq!(forward(&engine, "absent"), None);
}

#[test]
fn a_match_is_its_own_selection() {
    let engine = pane(b"cold store\r\n1\r\n2\r\n3\r\n4\r\n");
    let found = forward(&engine, "STORE");
    assert_eq!(
        found.map(|found| copied(&engine, found)).as_deref(),
        Some("store")
    );
}

#[test]
fn search_ignores_case() {
    let engine = pane("GRÜSSE Straße\r\n".as_bytes());
    assert!(forward(&engine, "grüsse").is_some());
    assert!(forward(&engine, "STRAßE").is_some());
}

#[test]
fn each_direction_takes_the_nearest_match() {
    let engine = pane(b"one x\r\ntwo x\r\nthree x\r\nfour\r\n5\r\n6\r\n");
    let first = forward(&engine, "x");
    assert_eq!(first.map(|found| found.anchor), Some(at(0, 4)));
    let last = backward(&engine, "x");
    assert_eq!(last.map(|found| found.anchor), Some(at(2, 6)));
    let below = find(&engine, "x", at(2, 6), Direction::Backward);
    assert_eq!(below.map(|found| found.anchor), Some(at(1, 4)));
    let above = find(&engine, "x", at(0, 5), Direction::Forward);
    assert_eq!(above.map(|found| found.anchor), Some(at(1, 4)));
}

#[test]
fn a_backward_match_stays_before_its_start() {
    let engine = pane(b"abcabc\r\n");
    let found = find(&engine, "abc", at(0, 5), Direction::Backward);
    assert_eq!(found.map(|found| found.anchor), Some(at(0, 0)));
}

#[test]
fn a_match_can_cross_a_soft_wrap_but_not_a_line_end() {
    let wrapped = format!("{}needle\r\n", "x".repeat(usize::from(COLUMNS) - 3));
    let engine = pane(wrapped.as_bytes());
    let found = forward(&engine, "needle");
    assert_eq!(
        found,
        Some(Selection {
            anchor: at(0, 17),
            focus: at(1, 2)
        })
    );
    assert_eq!(backward(&engine, "needle"), found);
    let engine = pane(b"nee\r\ndle\r\n");
    assert_eq!(forward(&engine, "needle"), None);
    assert_eq!(backward(&engine, "needle"), None);
}

#[test]
fn a_wrapped_match_survives_the_edge_of_the_store() {
    // The wrapped line leaves the screen one row at a time, so one output splits it in two.
    let wrapped = format!("{}needle", "x".repeat(usize::from(COLUMNS) - 3));
    let mut engine = pane(wrapped.as_bytes());
    feed(&mut engine, b"\r\n1\r\n2\r\n");
    assert_eq!(engine.position(0, 0).row, 1, "the wrap's first row is cold");
    assert_eq!(engine.scrollback().lock().unwrap().len(), 1);
    let found = Some(Selection {
        anchor: at(0, 17),
        focus: at(1, 2),
    });
    assert_eq!(forward(&engine, "needle"), found);
    assert_eq!(backward(&engine, "needle"), found);
    assert_eq!(
        found.map(|found| copied(&engine, found)).as_deref(),
        Some("needle")
    );
}

#[test]
fn a_match_can_cross_a_step() {
    let mut engine = AlacrittyEngine::new(ROWS, 10);
    let line = "y".repeat(10 * 100 - 3) + "needle\r\n1\r\n2\r\n3\r\n";
    feed(&mut engine, line.as_bytes());
    for row in 0..100 {
        let query = Search::new("needle", at(row, 0), Direction::Forward);
        let found = query.and_then(|search| run(&engine, search).0);
        let anchor = found.map(|found| found.anchor);
        assert_eq!(anchor, Some(at(99, 7)), "from {row}");
    }
    for row in 101..170 {
        let query = Search::new("needle", at(row, 0), Direction::Backward);
        let found = query.and_then(|search| run(&engine, search).0);
        assert_eq!(
            found.map(|found| found.focus),
            Some(at(100, 2)),
            "from {row}"
        );
    }
}

#[test]
fn a_wide_character_counts_as_one_in_a_match() {
    let engine = pane("a漢字b".as_bytes());
    let found = forward(&engine, "漢字b");
    assert_eq!(found.map(|found| found.anchor), Some(at(0, 1)));
    assert_eq!(found.map(|found| found.focus), Some(at(0, 5)));
}

#[test]
fn a_wide_character_pushed_to_the_next_row_still_joins() {
    let line = format!(
        "{}漢字\r\n1\r\n2\r\n3\r\n",
        "x".repeat(usize::from(COLUMNS) - 1)
    );
    let engine = pane(line.as_bytes());
    assert!(forward(&engine, "x漢字").is_some());
    assert_eq!(
        all(&engine).lines().next(),
        Some(&line[..line.find('\r').unwrap()])
    );
}

#[test]
fn a_search_steps_instead_of_reading_everything() {
    let engine = pane(numbers(1, 2000).as_bytes());
    let search = Search::new("absent", Position::default(), Direction::Forward).unwrap();
    assert!(run(&engine, search).1 > 20);
}

#[test]
fn output_cannot_keep_a_forward_search_running() {
    let mut engine = pane(numbers(1, 500).as_bytes());
    let mut search = Search::new("absent", Position::default(), Direction::Forward).unwrap();
    let mut steps = 0;
    while engine.search(&mut search) == SearchStep::Running {
        feed(&mut engine, numbers(1, 200).as_bytes());
        steps += 1;
        assert!(steps < 100, "the search must end");
    }
}

#[test]
fn eviction_during_a_search_is_survived() {
    let mut engine = pane(numbers(1, LINE_CAP).as_bytes());
    for direction in [Direction::Forward, Direction::Backward] {
        let start = at(
            if direction == Direction::Forward {
                0
            } else {
                7000
            },
            0,
        );
        let mut search = Search::new("absent", start, direction).unwrap();
        let mut steps = 0;
        while engine.search(&mut search) == SearchStep::Running {
            feed(&mut engine, numbers(1, 500).as_bytes());
            steps += 1;
            assert!(steps < 1000, "the search must end");
        }
    }
}

#[test]
fn a_join_never_crosses_rows_evicted_between_steps() {
    // The first step ends on "zz", and blocks are 64 rows, so whichever block eviction leaves
    // oldest starts with "dle", which must not count as the next row.
    let lines = (0..LINE_CAP).map(|row| match row % 64 {
        _ if row == 63 => "zz\r\n",
        0 => "dle\r\n",
        _ => "x\r\n",
    });
    let mut engine = pane(lines.collect::<String>().as_bytes());
    let mut search = Search::new("zz\ndle", Position::default(), Direction::Forward).unwrap();
    assert_eq!(engine.search(&mut search), SearchStep::Running);
    feed(&mut engine, "y\r\n".repeat(500).as_bytes());
    let mut step = SearchStep::Running;
    while step == SearchStep::Running {
        step = engine.search(&mut search);
    }
    assert_eq!(step, SearchStep::NotFound);
}

#[test]
fn clearing_history_ends_a_search_of_it() {
    let mut engine = pane(numbers(1, 40).as_bytes());
    let mut search = Search::new("7", at(30, 0), Direction::Backward).unwrap();
    feed(&mut engine, b"\x1b[3J\x1b[2J");
    assert_eq!(engine.search(&mut search), SearchStep::NotFound);
    assert_eq!(forward(&engine, "17"), None);
}

#[test]
fn the_alternate_screen_searches_only_itself() {
    let mut engine = pane(format!("hidden\r\n{}", numbers(1, 9)).as_bytes());
    assert!(forward(&engine, "hidden").is_some());
    feed(&mut engine, b"\x1b[?1049h\x1b[Hshown");
    assert_eq!(forward(&engine, "hidden"), None);
    assert!(forward(&engine, "shown").is_some());
    assert_eq!(all(&engine).lines().next(), Some("shown"));
}

#[test]
fn view_positions_follow_the_scroll() {
    let mut engine = pane(numbers(1, 9).as_bytes());
    assert_eq!(engine.position(0, 3), at(6, 3));
    engine.scroll(2);
    assert_eq!(engine.position(0, 3), at(4, 3));
    let found = forward(&engine, "9").unwrap();
    let top = engine.position(0, 0).row as i32;
    engine.scroll(top - found.anchor.row as i32);
    assert_eq!(engine.position(0, 0).row, found.anchor.row.min(6));
}

#[test]
fn selection_spans_history_and_the_screen() {
    let mut engine = pane(numbers(1, 9).as_bytes());
    let selection = Selection {
        anchor: at(5, 0),
        focus: at(7, 0),
    };
    assert_eq!(copied(&engine, selection), "6\n7\n8");
    feed(&mut engine, numbers(10, 40).as_bytes());
    assert_eq!(
        copied(&engine, selection),
        "6\n7\n8",
        "the rows moved, not the text"
    );
}

#[test]
fn selection_works_in_either_order_and_within_a_row() {
    let engine = pane(b"hello world\r\nnext\r\n");
    let selection = Selection {
        anchor: at(1, 1),
        focus: at(0, 6),
    };
    assert_eq!(copied(&engine, selection), "world\nne");
    let inside = Selection {
        anchor: at(0, 1),
        focus: at(0, 3),
    };
    assert_eq!(copied(&engine, inside), "ell");
}

#[test]
fn selection_joins_a_wrapped_line() {
    let long = "abcde".repeat(5);
    let line = format!("{long}\r\nnext\r\n1\r\n2\r\n3\r\n4\r\n");
    let engine = pane(line.as_bytes());
    assert_eq!(engine.scrollback().lock().unwrap().len(), 4);
    assert_eq!(
        all(&engine).lines().collect::<Vec<_>>()[..2],
        [&long, "next"]
    );
}

#[test]
fn selection_shows_wide_characters_once_in_history_and_live() {
    let line = "a漢字b\r\n1\r\n2\r\n3\r\n4\r\nc漢字d";
    let engine = pane(line.as_bytes());
    let cold = Selection {
        anchor: at(0, 0),
        focus: at(0, 19),
    };
    assert_eq!(copied(&engine, cold), "a漢字b");
    let live = Selection {
        anchor: at(5, 0),
        focus: at(5, 19),
    };
    assert_eq!(copied(&engine, live), "c漢字d");
    let halves = Selection {
        anchor: at(0, 2),
        focus: at(0, 3),
    };
    assert_eq!(
        copied(&engine, halves),
        "漢字",
        "a half selects the character"
    );
}

#[test]
fn a_spacer_left_in_the_first_column_is_not_a_crash() {
    // Deleting the first cell of a wide character slides its spacer into column 0.
    let engine = pane("\u{6f22}x\x1b[1G\x1b[P".as_bytes());
    let row = Selection {
        anchor: at(0, 0),
        focus: at(0, 5),
    };
    assert_eq!(copied(&engine, row), "x");
}

#[test]
fn selection_keeps_tabs_marks_and_drops_trailing_blanks() {
    let line = "a\tb  \x1b[41m   \x1b[0m\r\ne\u{301}\r\n1\r\n2\r\n3\r\n4\r\n";
    let engine = pane(line.as_bytes());
    assert_eq!(engine.scrollback().lock().unwrap().len(), 3);
    let selection = Selection {
        anchor: at(0, 0),
        focus: at(1, 19),
    };
    assert_eq!(copied(&engine, selection), "a\tb\ne\u{301}");
}

#[test]
fn eviction_clamps_a_selection_to_the_oldest_row() {
    let mut engine = pane(numbers(1, 100).as_bytes());
    let selection = Selection {
        anchor: at(0, 0),
        focus: at(5, 0),
    };
    assert_eq!(copied(&engine, selection), "1\n2\n3\n4\n5\n6");
    let reaches = Selection {
        anchor: at(0, 3),
        focus: at(99, 0),
    };
    feed(&mut engine, numbers(1, LINE_CAP * 2).as_bytes());
    assert_eq!(copied(&engine, selection), "", "every row of it is gone");
    let total = engine.scrollback().lock().unwrap().end();
    let tail = Selection {
        focus: at(total, 0),
        ..reaches
    };
    let text = copied(&engine, tail);
    let oldest: usize = text.lines().next().unwrap().parse().unwrap();
    assert!(oldest > 1, "clamped to a row still held");
}

#[test]
fn selection_past_the_end_stops_at_the_screen() {
    let engine = pane(b"one");
    let selection = Selection {
        anchor: at(0, 0),
        focus: at(usize::MAX, u16::MAX),
    };
    assert_eq!(copied(&engine, selection), "one\n\n\n");
    let beyond = Selection {
        anchor: at(99, 0),
        focus: at(usize::MAX, u16::MAX),
    };
    assert_eq!(copied(&engine, beyond), "");
}

#[test]
fn clearing_history_empties_a_selection_of_it() {
    let mut engine = pane(numbers(1, 40).as_bytes());
    let selection = Selection {
        anchor: at(1, 0),
        focus: at(9, 0),
    };
    feed(&mut engine, b"\x1b[3J");
    assert_eq!(copied(&engine, selection), "");
}

#[test]
fn selection_size_is_bounded_on_a_row() {
    let mut engine = AlacrittyEngine::new(ROWS, 1024);
    let row = "e\u{301}\u{301}\u{301}".repeat(1024) + "\r\n";
    feed(&mut engine, row.repeat(400).as_bytes());
    let selection = Selection {
        anchor: Position::default(),
        focus: at(usize::MAX, u16::MAX),
    };
    let mut text = String::new();
    let fits = engine.selection_text(selection, &mut text);
    assert!(!fits, "{} bytes", text.len());
    assert!(text.len() <= MAX_SELECTION);
    assert!(text.ends_with('\n'), "cut at a row boundary");
}

fn cells() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop::sample::select(vec![
            "a",
            "A",
            "é",
            "漢",
            "\t",
            " ",
            "\r\n",
            "\n",
            "e\u{301}",
            "ß",
            "\x1b[1G\x1b[P",
            "\x1b[41m",
            "\x1b[3J",
            "\x1b[?1049h",
            "\x1b[?1049l",
            "\x1b[2J",
            "\x1bc",
        ]),
        0..400,
    )
    .prop_map(|parts| parts.concat())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn no_output_or_query_breaks_a_search_or_selection(
        output in cells(),
        more in cells(),
        query in "[aA é漢\n]{1,12}",
        anchor in (any::<usize>(), any::<u16>()),
        focus in (0..600_usize, 0..1024_u16),
        columns in 2..30_u16,
    ) {
        let mut engine = AlacrittyEngine::new(ROWS, columns);
        feed(&mut engine, output.as_bytes());
        let anchor = at(anchor.0, anchor.1);
        let focus = at(focus.0, focus.1);
        for direction in [Direction::Forward, Direction::Backward] {
            for start in [anchor, focus] {
                let Some(mut search) = Search::new(&query, start, direction) else {
                    continue;
                };
                engine.search(&mut search);
                feed(&mut engine, more.as_bytes());
                engine.resize(ROWS + 1, columns);
                run(&engine, search);
            }
        }
        let mut text = String::new();
        engine.selection_text(Selection { anchor, focus }, &mut text);
        prop_assert!(text.len() <= MAX_SELECTION);
    }

    #[test]
    fn search_finds_what_a_row_holds_from_either_end(
        before in "[a-c]{0,40}",
        after in "[a-c]{0,40}",
        history in 0..200_usize,
    ) {
        let line = format!("{before}NEEDLE{after}\r\n");
        let engine = pane([line, numbers(1, history)].concat().as_bytes());
        let from = forward(&engine, "needle");
        let back = backward(&engine, "needle");
        prop_assert!(from.is_some());
        prop_assert_eq!(from, back);
        let found = from.map(|found| copied(&engine, found));
        prop_assert_eq!(found.as_deref(), Some("NEEDLE"));
    }
}
