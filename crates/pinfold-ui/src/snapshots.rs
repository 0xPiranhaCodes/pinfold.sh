//! Snapshots of the chrome as cells: pane borders, the bar with its status line, an approval
//! prompt, the right-click menu and the command palette.

use pinfold_config::theme::GRAPHITE;
use std::fmt::Write as _;
use std::time::Instant;

use pinfold_proto::link::{ApprovalAction, Rect};
use pinfold_proto::{BOLD, Cell, Frame, ITALIC, TALL};
use pinfold_sys::LocalTime;

use crate::approval::Queue;
use crate::context_menu::Menu;
use crate::link::Approval;
use crate::palette::Palette;
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
    dividers(&mut grid, &rects, Some(rects[2]), &GRAPHITE);
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
        theme: &GRAPHITE,
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
        unhardened: None,
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

/// A tool pane's tab, active beside a running one: its title, and no agent state, as the core
/// says a tool pane is idle and never reported (ADR-0045).
fn tool_bar() -> Frame {
    let mut label = String::new();
    top_bar::label("files pinfold", &mut label);
    let tabs = ["work", label.as_str()];
    // Running and idle, as `tab_state` numbers them.
    let states = [1, 4];
    let mut frame = pane(' ', 1, 60);
    let bar = Bar {
        theme: &GRAPHITE,
        tabs: &tabs,
        active: 1,
        activity: top_bar::marked(&states),
        states: &states,
        status: Some((4, false, "")),
        hovered: None,
        focused: true,
        title: true,
        time: None,
        inset: 1,
        unhardened: None,
    };
    top_bar::draw(&mut frame, &bar);
    frame
}

#[test]
fn a_tool_panes_tab_shows_its_title_and_no_agent_state() {
    let frame = tool_bar();
    let title: String = frame.cells[..60]
        .iter()
        .map(|cell| cell.character)
        .collect();
    assert_eq!(title.trim(), "files pinfold");
    insta::with_settings!({snapshot_suffix => std::env::consts::OS}, {
        insta::assert_snapshot!(cells(&frame));
    });
}

/// A core without the hardened runtime: its notice under the pane and the bar's marker. Only
/// macOS shows them, so only macOS has the snapshot and image.
#[cfg(target_os = "macos")]
pub(crate) fn unhardened() -> Frame {
    let tabs = ["work", "logs"];
    let mut frame = pane(' ', 1, 80);
    let mut flash = crate::flash::Flash::default();
    let command =
        "codesign -f -s - -o runtime '/Users/me/pinfold/target/release'/pinfold-{core,window}";
    flash.unhardened(true, command.to_owned(), Instant::now());
    flash.banner(&mut frame, &GRAPHITE);
    let bar = Bar {
        theme: &GRAPHITE,
        tabs: &tabs,
        active: 0,
        activity: 0,
        states: &[],
        status: None,
        hovered: None,
        focused: true,
        title: true,
        time: None,
        inset: 1,
        unhardened: Some(true),
    };
    top_bar::draw(&mut frame, &bar);
    frame
}

#[test]
#[cfg(target_os = "macos")]
fn an_unhardened_core_has_a_notice_and_a_marker() {
    insta::assert_snapshot!(cells(&unhardened()));
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
    queue.draw(&mut frame, Instant::now(), &GRAPHITE);
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
    menu.draw(&mut frame, &GRAPHITE);
    insta::assert_snapshot!(cells(&frame));
}

#[test]
fn the_command_palette_filtered_to_open() {
    let mut frame = pane('x', 12, 60);
    let mut palette = Palette::commands();
    let open = winit::keyboard::Key::Character("open".into());
    palette.key(
        &open,
        Some("open"),
        winit::keyboard::ModifiersState::empty(),
    );
    palette.draw(&mut frame, &GRAPHITE);
    insta::assert_snapshot!(cells(&frame));
}

/// ADR-0049: every colour of the chrome is read from the theme. Under a light theme with only
/// its 18 required keys, no piece of chrome draws a dark ground or one of Graphite Grid's
/// colours, which a constant left behind would.
#[test]
fn a_light_theme_colours_every_piece_of_chrome() {
    use pinfold_config::theme::{Colors, parse};

    use crate::paste::{Prompt, banner};

    // GitHub Light's background, foreground and cyan, as in `docs/spikes/themes/contrast.py`.
    let mut file = String::from("background = \"#ffffff\"\nforeground = \"#1f2328\"\n");
    for key in [
        "black", "red", "green", "yellow", "blue", "magenta", "white",
    ] {
        let _ = writeln!(file, "{key} = \"#808080\"\nbright_{key} = \"#808080\"");
    }
    file += "cyan = \"#1b7c83\"\nbright_cyan = \"#808080\"\n";
    let (theme, notes) = parse(file.as_bytes(), Colors::default()).unwrap();
    assert_eq!(notes, [""; 0], "its own chrome, not the fallback");
    let screen = |rows, columns| {
        let mut frame = Frame::default();
        crate::panes::clear(&mut frame, rows, columns, theme.background);
        frame
    };
    let whole = |rows, columns| Rect {
        x: 0,
        y: 0,
        columns,
        rows,
    };
    let mut drawn = Vec::new();

    let mut frame = screen(5, 9);
    let rects = [
        Rect {
            columns: 4,
            ..whole(5, 9)
        },
        Rect {
            x: 5,
            ..whole(5, 4)
        },
    ];
    dividers(&mut frame, &rects, Some(rects[0]), &theme);
    let (tabs, states) = (["work", "logs", "agent", "done", "bad"], [1, 4, 0, 2, 3]);
    for (focused, hovered) in [(true, Some(1)), (false, None)] {
        let bar = Bar {
            tabs: &tabs,
            active: 2,
            activity: top_bar::marked(&states),
            states: &states,
            status: Some((0, true, "approve the diff?")),
            hovered,
            focused,
            title: true,
            time: Some(LocalTime {
                weekday: 5,
                day: 6,
                month: 10,
                hour: 9,
                minute: 41,
                second: 0,
            }),
            inset: 1,
            unhardened: None,
            theme: &theme,
        };
        let mut frame = screen(1, 90);
        top_bar::draw(&mut frame, &bar);
        drawn.push(frame);
    }
    drawn.push(frame);

    let mut frame = screen(14, 20);
    Menu::open((1, 2), (14, 20), Some("https://pinfold.sh".to_owned())).draw(&mut frame, &theme);
    drawn.push(frame);
    let mut frame = screen(12, 60);
    Palette::commands().draw(&mut frame, &theme);
    drawn.push(frame);

    let mut queue = Queue::default();
    let send = Approval {
        id: 7,
        caller: 2,
        target: 0,
        action: ApprovalAction::Send,
        texts: vec!["cargo test\n".to_owned()],
    };
    assert_eq!(queue.add(send), None);
    let mut frame = screen(6, 80);
    queue.draw(&mut frame, Instant::now(), &theme);
    drawn.push(frame);

    let prompts = [
        Prompt::Confirm {
            text: "rm -rf /\n".to_owned(),
            lines: 1,
            risk: Some("ends a line"),
        },
        Prompt::TooLarge,
        Prompt::Close {
            pane: 1,
            last: true,
        },
        Prompt::OpenLink("file:///etc/passwd".to_owned()),
    ];
    for prompt in &prompts {
        let mut frame = screen(6, 80);
        banner(&mut frame, prompt, &theme);
        drawn.push(frame);
    }
    let mut frame = screen(2, 80);
    crate::flash::ended(&mut frame, &theme);
    drawn.push(frame);
    let mut frame = screen(2, 80);
    crate::was_running::banner(&mut frame, true, "cargo test", &theme);
    drawn.push(frame);

    let graphite = [
        GRAPHITE.foreground,
        GRAPHITE.chrome.accent,
        GRAPHITE.chrome.dim,
        GRAPHITE.chrome.muted,
        GRAPHITE.chrome.tab_text,
        GRAPHITE.chrome.key,
        GRAPHITE.chrome.title,
        GRAPHITE.chrome.title_unfocused,
        GRAPHITE.chrome.rule,
        GRAPHITE.chrome.border,
        GRAPHITE.states.needs_you,
        GRAPHITE.states.running,
        GRAPHITE.states.done,
        GRAPHITE.states.error,
        GRAPHITE.states.idle,
    ];
    for (index, frame) in drawn.iter().enumerate() {
        for cell in &frame.cells {
            let light = cell.background.to_be_bytes()[1..]
                .iter()
                .all(|&c| c >= 0x80);
            assert!(light, "frame {index}: a dark ground, {}", describe(*cell));
            let text = cell.foreground & 0x00ff_ffff;
            assert!(
                !graphite.contains(&text),
                "frame {index}: {}",
                describe(*cell)
            );
        }
    }
}
