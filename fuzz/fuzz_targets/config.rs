//! Arbitrary user config or theme bytes must always be parsed or refused without a panic.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = pinfold_config::parse(data);
    let _ = pinfold_config::theme::parse(data, pinfold_config::theme::Colors::default());
});
