//! Snapshots of the chrome as cells: pane borders, the bar with its status line, an approval
//! prompt and the right-click menu.

use std::fmt::Write as _;
use std::time::Instant;

use pinfold_proto::link::{ApprovalAction, Rect};
use pinfold_proto::{BOLD, Cell, Frame, ITALIC, TALL};
use pinfold_sys::LocalTime;

use crate::approval::Queue;
use crate::context_menu::Menu;
use crate::link::Approval;
use crate::panes::{blank, blit, dividers};
use crate::top_bar::{self, Bar};

/// Each row between bars, then each run of cells whose colours or style differ from the cell
/// before it, as `row:start..end`.
fn cells(frame: &Frame) -> String {
    let mut shown = String::new();
    let rows = frame.cells.chunks(usize::from(frame.columns));
    for row in rows.clone() {
        let text: String = row.iter().map(|cell| cell.character).collect();
        let _ = writeln!(shown, "|{text}|");
    }
    let look = |cell: &Cell| (cell.foreground, cell.background);
    for (line, row) in rows.enumerate() {
        let mut start = 0;
        while start < row.len() {
            let run = row[start..]
                .iter()
                .take_while(|cell| look(cell) == look(&row[start]));
            let end = start + run.count();
            let _ = writeln!(shown, "{line}:{start}..{end} {}", describe(row[start]));
            start = end;
        }
    }
    shown
}

fn describe(cell: Cell) -> String {
    let colour = cell.foreground & 0x00ff_ffff;
    let mut text = format!("fg {colour:06x} bg {:06x}", cell.background);
    for (bit, name) in [(BOLD, " bold"), (ITALIC, " italic"), (TALL, " tall")] {
        if cell.style() & bit != 0 {
            text.push_str(name);
        }
    }
    text
}

pub(crate) fn pane(character: char, rows: u16, columns: u16) -> Frame {
    let cell = Cell {
        character,
        foreground: 0x00e2_e5ea,
        background: 0x0012_1212,
    };
    Frame {
        rows,
        columns,
        cells: vec![cell; usize::from(rows) * usize::from(columns)],
        ..Frame::default()
    }
}

/// Three panes, the focused one accented.
pub(crate) fn borders() -> Frame {
    let rect = |x, y, columns, rows| Rect {
        x,
        y,
        columns,
        rows,
    };
    let rects = [rect(0, 0, 3, 5), rect(4, 0, 4, 2), rect(4, 3, 4, 2)];
    let mut grid = Frame::default();
    blank(&mut grid, 5, 8);
    for (rect, character) in rects.iter().zip(['a', 'b', 'c']) {
        blit(&mut grid, &pane(character, rect.rows, rect.columns), *rect);
    }
    dividers(&mut grid, &rects, Some(rects[2]));
    grid
}

#[test]
fn borders_between_three_panes_with_the_focused_one_accented() {
    insta::assert_snapshot!(cells(&borders()));
}

/// Tabs in three states and a status line.
pub(crate) fn bar() -> Frame {
    let tabs = ["work", "logs", "agent"];
    // Running, idle and needs you, as `tab_state` numbers them.
    let states = [1, 4, 0];
    let mut frame = pane(' ', 1, 60);
    let time = LocalTime {
        weekday: 5,
        day: 6,
        month: 10,
        hour: 9,
        minute: 41,
        second: 0,
    };
    let bar = Bar {
        tabs: &tabs,
        active: 2,
        activity: top_bar::marked(&states),
        states: &states,
        status: Some((0, true, "approve the diff?")),
        hovered: None,
        focused: true,
        title: true,
        time: Some(time),
        inset: 1,
    };
    top_bar::draw(&mut frame, &bar);
    frame
}

#[test]
fn the_bar_shows_tabs_states_and_the_status_line() {
    let frame = bar();
    // A tab's key hint is `⌘1` on macOS and `1` elsewhere, so each system has its own snapshot.
    insta::with_settings!({snapshot_suffix => std::env::consts::OS}, {
        insta::assert_snapshot!(cells(&frame));
    });
}

/// A prompt to approve one pane sending to another.
pub(crate) fn approval() -> Frame {
    let mut queue = Queue::default();
    let send = Approval {
        id: 7,
        caller: 2,
        target: 0,
        action: ApprovalAction::Send,
        texts: vec!["cargo test\n".to_owned()],
    };
    assert_eq!(queue.add(send), None);
    let mut frame = pane('x', 6, 80);
    queue.draw(&mut frame, Instant::now());
    frame
}

#[test]
fn an_approval_prompt_names_both_panes_and_the_exact_send() {
    insta::assert_snapshot!(cells(&approval()));
}

#[test]
fn the_right_click_menu_with_a_link() {
    let mut frame = pane('x', 14, 20);
    let menu = Menu::open((1, 2), (14, 20), Some("https://pinfold.sh".to_owned()));
    menu.draw(&mut frame);
    insta::assert_snapshot!(cells(&frame));
}
