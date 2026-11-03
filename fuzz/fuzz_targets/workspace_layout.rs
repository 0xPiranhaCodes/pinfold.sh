//! Arbitrary repository layout bytes must always be parsed or refused without a panic.

#![no_main]

use std::path::Path;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = pinfold_config::workspace::parse(data, Path::new("."));
});
