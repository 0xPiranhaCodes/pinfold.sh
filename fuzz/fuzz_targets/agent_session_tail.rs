//! Any bytes as the tail of a Claude Code session file, read as `pinfold agents peek` reads it,
//! never panic and print one line of at most 1 KiB with no control or bidi character in it
//! (ADR-0051 §7).

#![no_main]

use libfuzzer_sys::fuzz_target;
use pinfold_tools::fuzz_agent_session_tail;

fuzz_target!(|data: &[u8]| {
    // The first byte says whether the tail starts at the file's start.
    let (whole, tail) = data.split_first().map_or((true, data), |(&first, rest)| (first & 1 == 1, rest));
    let line = fuzz_agent_session_tail(tail, whole);
    assert!(line.len() <= 1024);
    assert!(!line.chars().any(|c| c.is_control()
        || matches!(c, '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')));
});
