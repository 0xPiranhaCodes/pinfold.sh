//! Limits on what program output can make a terminal hold (T3 in the threat model).

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

struct Size(usize, usize);

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.0
    }

    fn screen_lines(&self) -> usize {
        self.0
    }

    fn columns(&self) -> usize {
        self.1
    }
}

#[derive(Clone, Default)]
struct Titles(Arc<Mutex<Vec<String>>>);

impl EventListener for Titles {
    fn send_event(&self, event: Event) {
        if let Event::Title(title) = event
            && let Ok(mut titles) = self.0.lock()
        {
            titles.push(title);
        }
    }
}

fn pane(lines: usize, columns: usize) -> (Term<Titles>, Titles) {
    let titles = Titles::default();
    let term = Term::new(Config::default(), &Size(lines, columns), titles.clone());
    (term, titles)
}

fn feed(term: &mut Term<Titles>, bytes: &[u8]) {
    Processor::<StdSyncHandler>::new().advance(term, bytes);
}

fn links_on_screen(term: &Term<Titles>) -> usize {
    let grid = term.grid();
    (0..grid.screen_lines())
        .flat_map(|line| (0..grid.columns()).map(move |column| (line, column)))
        .filter(|&(line, column)| {
            grid[Line(line as i32)][Column(column)]
                .hyperlink()
                .is_some()
        })
        .count()
}

#[test]
fn long_titles_are_cut_at_a_character() {
    let (mut term, titles) = pane(4, 20);
    let title = "漢".repeat(200);
    feed(&mut term, format!("\x1b]2;{title}\x07").as_bytes());
    let titles = titles.0.lock().unwrap();
    assert_eq!(titles[0].len(), 255);
    assert!(title.starts_with(&titles[0]));
}

#[test]
fn the_title_stack_is_shallow() {
    let (mut term, titles) = pane(4, 20);
    feed(&mut term, b"\x1b]2;title\x07");
    feed(&mut term, b"\x1b[22t".repeat(100).as_slice());
    titles.0.lock().unwrap().clear();
    feed(&mut term, b"\x1b[23t".repeat(100).as_slice());
    assert_eq!(titles.0.lock().unwrap().len(), 16);
}

#[test]
fn hyperlinks_stop_at_the_limit_and_return_when_they_scroll_away() {
    let (mut term, _) = pane(200, 200);
    let mut bytes = Vec::new();
    for link in 0..5000 {
        bytes.extend_from_slice(format!("\x1b]8;;https://e.com/{link}\x07x").as_bytes());
    }
    feed(&mut term, &bytes);
    assert_eq!(links_on_screen(&term), 2000);

    feed(&mut term, b"\x1b]8;;\x07\x1b[2J\x1b[H");
    assert_eq!(links_on_screen(&term), 0);
    feed(
        &mut term,
        b"\x1b]8;;https://e.com/again\x07y".repeat(2001).as_slice(),
    );
    assert!(links_on_screen(&term) > 0);
}
