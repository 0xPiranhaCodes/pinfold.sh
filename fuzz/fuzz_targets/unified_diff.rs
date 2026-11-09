//! Any bytes, parsed as `git diff` output by the diff viewer, never panic, and every label and
//! line it shows holds no unescaped control or bidi character.

#![no_main]

use libfuzzer_sys::fuzz_target;
use pinfold_tools::fuzz_unified_diff;
use pinfold_tools::screen::escape;

fuzz_target!(|data: &[u8]| {
    for line in fuzz_unified_diff(data) {
        // Escaped text escapes to itself, so this holds only if nothing was left to escape.
        assert_eq!(escape(line.as_bytes()), line);
    }
});
