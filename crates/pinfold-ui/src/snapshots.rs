//! Snapshots of the chrome as cells: pane borders, the bar with its status line, an approval
//! prompt, the right-click menu, the command palette, the inbox and its answer prompt, and the
//! timeline.

use pinfold_config::theme::GRAPHITE;
use std::fmt::Write as _;
use std::time::Instant;

use pinfold_proto::link::{ApprovalAction, Rect};
use pinfold_proto::link::{FAILED, INDETERMINATE, NORMAL, PAUSED, progress};
use pinfold_proto::{BAR, BOLD, Cell, Frame, ITALIC, TALL};
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
    let mut text = format!("fg {colour:06x} bg {:06x}", cell.background & !BAR);
    for (bit, name) in [(BOLD, " bold"), (ITALIC, " italic"), (TALL, " tall")] {
        if cell.style() & bit != 0 {
            text.push_str(name);
        }
    }
    if cell.background & BAR != 0 {
        text.push_str(" bar");
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
        progress: &[],
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

/// ADR-0051 §9: a tab's label and its progress bar in each state, a row each: none, normal at
/// 50 % and at 100 %, error, no percent and paused. Only the tab's end and label, which every
/// system draws alike.
pub(crate) fn progress_tabs() -> Frame {
    const CELLS: u16 = 11;
    let bytes = [
        0,
        progress(NORMAL, 50),
        progress(NORMAL, 100),
        progress(FAILED, 50),
        progress(INDETERMINATE, 0),
        progress(PAUSED, 25),
    ];
    let mut rows = pane(' ', 0, CELLS);
    for byte in bytes {
        let mut frame = pane(' ', 1, 40);
        let bar = Bar {
            theme: &GRAPHITE,
            tabs: &["building"],
            active: 0,
            activity: 0,
            states: &[],
            status: None,
            hovered: None,
            focused: true,
            title: false,
            time: None,
            inset: 1,
            unhardened: None,
            progress: &[byte],
        };
        top_bar::draw(&mut frame, &bar);
        rows.cells
            .extend_from_slice(&frame.cells[..usize::from(CELLS)]);
        rows.rows += 1;
    }
    rows
}

#[test]
fn a_tabs_progress_is_a_bar_under_its_label_in_each_state() {
    insta::assert_snapshot!(cells(&progress_tabs()));
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
        progress: &[],
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
        progress: &[],
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

/// PinFold's rewind prompt as the core words it: the directory, since when and why, who asks,
/// the counts, a note, and each path, the one changed later in red (ADR-0050 section 6).
pub(crate) fn rewind() -> Frame {
    let mut queue = Queue::default();
    let texts = [
        "/Users/me/work/pinfold",
        "since 12 min ago (launch)",
        "asked by the tool in pane 3",
        "restore 1 · delete 1 the agent added · bring back 1 it deleted · 1 changed later",
        "the agent is still running, and may change these files again",
        "! restore src/main.rs",
        "  delete notes^[[2J.txt",
        "  bring back README.md",
        "and 2 more",
    ];
    let rewind = Approval {
        id: 9,
        caller: 0,
        target: 3,
        action: ApprovalAction::Rewind,
        texts: texts.map(str::to_owned).to_vec(),
    };
    assert_eq!(queue.add(rewind), None);
    let mut frame = pane('x', 12, 80);
    queue.draw(&mut frame, Instant::now(), &GRAPHITE);
    frame
}

#[test]
fn a_rewind_prompt_shows_the_counts_and_the_paths_with_later_ones_in_red() {
    insta::assert_snapshot!(cells(&rewind()));
}

/// The strip under an agent's pane whose launch checkpoint took over `wait_s`, in the muted
/// chrome colour (ADR-0050, slice 6 amendment).
#[test]
fn the_strip_says_a_run_has_no_checkpoint_and_why() {
    let mut frame = pane('x', 2, 80);
    crate::flash::no_checkpoint(&mut frame, 1, &GRAPHITE);
    insta::assert_snapshot!(cells(&frame));
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
    let mut palette = Palette::commands(false, false);
    let open = winit::keyboard::Key::Character("open".into());
    palette.key(
        &open,
        Some("open"),
        winit::keyboard::ModifiersState::empty(),
    );
    palette.draw(&mut frame, &GRAPHITE);
    insta::assert_snapshot!(cells(&frame));
}

/// The inbox over a pane: five agents in three tabs, ◆ first, the ◆ row focused (ADR-0051).
pub(crate) fn inbox() -> Frame {
    let mut frame = pane('x', 14, 92);
    let now = Instant::now();
    crate::inbox::tests::board(now).draw(&mut frame, &GRAPHITE, now, None);
    frame
}

#[test]
fn the_inbox_needs_you_first() {
    insta::assert_snapshot!(cells(&inbox()));
}

/// The inbox with each run's summary over a pane whose program printed a fake row and
/// summary (ADR-0052 §5): the fake never reaches the inbox's rows.
pub(crate) fn inbox_runs() -> Frame {
    let mut frame = pane(' ', 14, 100);
    let fake = "│ ✓ done 3·1 infra 99 commands · 0 errors · t timeline │ ";
    for (cell, character) in frame.cells.iter_mut().zip(fake.chars().cycle()) {
        cell.character = character;
    }
    let now = Instant::now();
    crate::inbox::tests::with_runs(now).draw(&mut frame, &GRAPHITE, now, None);
    frame
}

#[test]
fn the_inbox_shows_each_runs_summary() {
    insta::assert_snapshot!(cells(&inbox_runs()));
}

/// A restored agent's `was running` strip with its run's summary, under a pane whose program
/// printed a fake strip (ADR-0052 §3): the real one is PinFold's own row.
pub(crate) fn was_running() -> Frame {
    let mut frame = pane(' ', 2, 120);
    let fake = " ○ was running: rm -rf ~ · 99 commands · 0 errors  Enter run · Esc dismiss ";
    for (cell, character) in frame.cells.iter_mut().zip(fake.chars().cycle()) {
        cell.character = character;
    }
    let line = "12 commands · 3 approvals · 8 files changed · 1 error";
    crate::was_running::banner(&mut frame, true, "claude", line, &GRAPHITE);
    frame
}

#[test]
fn the_was_running_strip_says_what_the_run_did() {
    insta::assert_snapshot!(cells(&was_running()));
}

/// The timeline over a pane whose program printed a fake one, every row a hostile copy of
/// its chrome and keys: a run with every kind, a hostile label and an evicted row (ADR-0052 §5).
pub(crate) fn timeline(columns: u16) -> Frame {
    let mut frame = pane(' ', 24, columns);
    let fake = "│ TIMELINE · ◆ needs you · ⏎ jump · d diff · y allow once │ ";
    for (cell, character) in frame.cells.iter_mut().zip(fake.chars().cycle()) {
        cell.character = character;
    }
    let (timeline, head) = crate::timeline::tests::run();
    timeline.draw(&mut frame, &GRAPHITE, &head);
    frame
}

#[test]
fn the_timeline_at_80_columns() {
    insta::assert_snapshot!(cells(&timeline(80)));
}

#[test]
fn the_timeline_at_120_columns() {
    insta::assert_snapshot!(cells(&timeline(120)));
}

/// PinFold's confirmation of Approve for the inbox's ◆ row, drawn over it as a trust prompt is
/// (ADR-0051 §6).
pub(crate) fn answer() -> Frame {
    use pinfold_proto::link::Choice;

    let mut frame = inbox();
    let confirm = crate::approval::Confirm::new(
        (2, 0, 1),
        Choice::Approve,
        ("claude", "Enter"),
        Instant::now(),
    );
    confirm.draw(&mut frame, &GRAPHITE);
    frame
}

#[test]
fn the_answer_prompt_names_the_key_the_profile_and_the_pane() {
    insta::assert_snapshot!(cells(&answer()));
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
            progress: &[],
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
    Palette::commands(false, false).draw(&mut frame, &theme);
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
    crate::was_running::banner(&mut frame, true, "cargo test", "", &theme);
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
