//! Arbitrary method parameters must never panic the core's request parser.

#![no_main]

use libfuzzer_sys::fuzz_target;
use pinfold_api::control::fuzz_request_line;

fuzz_target!(|data: &[u8]| {
    let mut request = br#"{"jsonrpc":"2.0","id":1,"method":"hello","params":{"version":"#.to_vec();
    request.extend_from_slice(data);
    request.extend_from_slice(b"}}");
    fuzz_request_line(&request);
});
