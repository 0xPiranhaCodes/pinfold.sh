//! Arbitrary bytes must always be parsed or refused as an iTerm2 `.itermcolors` file without
//! a panic.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = pinfold_config::itermcolors::parse(data, pinfold_config::theme::Colors::default());
});
