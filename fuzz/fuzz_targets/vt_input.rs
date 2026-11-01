//! Program output through a pane's terminal, its scrollback and resizes.
//!
//! The first two bytes set the size. `0xf5` never appears in UTF-8, so it splits the rest
//! into reads with a resize between them.

#![no_main]

use libfuzzer_sys::fuzz_target;
use pinfold_core::scrollback::BYTE_CAP;
use pinfold_core::vt::{AlacrittyEngine, VtEngine};
use pinfold_proto::Frame;

fuzz_target!(|data: &[u8]| {
    let [rows, columns, rest @ ..] = data else {
        return;
    };
    let mut engine = AlacrittyEngine::new(u16::from(*rows % 64), u16::from(*columns));
    let (mut replies, mut frame) = (Vec::new(), Frame::default());
    for read in rest.split(|&byte| byte == 0xf5) {
        engine.feed(read, &mut replies);
        engine.frame(&mut frame);
        let (rows, columns) = engine.size();
        assert_eq!(frame.cells.len(), usize::from(rows) * usize::from(columns));
        (0..=rows).for_each(|line| drop(engine.line_text(line)));
        let width = read.first().map_or(1, |&byte| u16::from(byte));
        engine.resize(read.len() as u16 % 64, width);
    }

    let store = engine.scrollback().lock().unwrap();
    assert!(store.bytes() <= BYTE_CAP);
    // `rows` hides a block it cannot decode, so a short answer is how a bad block shows.
    assert_eq!(store.rows(0, store.len()).len(), store.len());
});
