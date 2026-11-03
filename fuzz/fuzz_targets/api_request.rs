//! Arbitrary control-socket request lines must never panic the core.

#![no_main]

use libfuzzer_sys::fuzz_target;
use pinfold_api::control::fuzz_request_line;

fuzz_target!(|data: &[u8]| fuzz_request_line(data));
