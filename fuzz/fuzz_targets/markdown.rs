//! Any bytes, laid out by the markdown reader at a width from 1 to 300, never panic, and the
//! laid-out text holds no unescaped control or bidi character.

#![no_main]

use libfuzzer_sys::fuzz_target;
use pinfold_tools::fuzz_markdown;
use pinfold_tools::screen::escape;

fuzz_target!(|data: &[u8]| {
    let Some((&[low, high], markdown)) = data.split_first_chunk() else {
        return;
    };
    let width = u16::from_le_bytes([low, high]) % 300 + 1;
    for line in fuzz_markdown(markdown, width) {
        // Escaped text escapes to itself, so this holds only if nothing was left to escape.
        assert_eq!(escape(line.as_bytes()), line);
    }
});
