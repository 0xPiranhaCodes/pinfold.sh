//! Any bytes on the window link must end in an error, or in messages that encode back to them.

#![no_main]

use libfuzzer_sys::fuzz_target;
use pinfold_proto::Frame;
use pinfold_proto::link::{Decoder, MAX_DIMENSION, MAX_PANES, MAX_TABS, Notice, Request, VERSION};

fuzz_target!(|data: &[u8]| {
    let Some((&chunk, data)) = data.split_first() else {
        return;
    };
    let chunk = usize::from(chunk).max(1);
    // Past the version check, so the mutator reaches every other message.
    let mut wire = Vec::new();
    Request::Hello { version: VERSION }.encode(&mut wire).unwrap();
    wire.extend_from_slice(data);

    let (mut decoder, mut rebuilt) = (Decoder::default(), Vec::new());
    'requests: for part in wire.chunks(chunk) {
        let mut input = part;
        loop {
            match decoder.next_request(&mut input) {
                Ok(Some(request)) => request.encode(&mut rebuilt).unwrap(),
                Ok(None) => break,
                Err(_) => break 'requests,
            }
        }
    }
    assert!(wire.starts_with(&rebuilt));

    let (mut decoder, mut rebuilt) = (Decoder::default(), Vec::new());
    let mut drawn = Frame::default();
    'notices: for part in wire.chunks(chunk) {
        let mut input = part;
        loop {
            match decoder.next_notice(&mut input) {
                Ok(Some(notice)) => {
                    notice.encode(&mut rebuilt).unwrap();
                    if let Notice::Frame(update) = &notice {
                        if update.apply(&mut drawn) {
                            let cells = usize::from(drawn.rows) * usize::from(drawn.columns);
                            assert_eq!(drawn.cells.len(), cells);
                        }
                    }
                    if let Notice::Layout(layout) = &notice {
                        assert!(layout.tabs().count() <= MAX_TABS);
                        assert!(layout.panes().count() <= usize::from(layout.total));
                        assert!(usize::from(layout.total) <= MAX_PANES);
                        assert!(usize::from(layout.active) < layout.tabs().count().max(1));
                        assert!(layout.panes().all(|(_, rect)| {
                            rect.x + rect.columns <= MAX_DIMENSION
                                && rect.y + rect.rows <= MAX_DIMENSION
                        }));
                    }
                }
                Ok(None) => break,
                Err(_) => break 'notices,
            }
        }
    }
    assert!(wire.starts_with(&rebuilt));
});
