//! Pasted text must come out with no way to act as anything but text.

#![no_main]

use libfuzzer_sys::fuzz_target;
use pinfold_core::paste::encode;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let plain = String::from_utf8(encode(&text, false)).unwrap();
    assert!(!plain.contains('\n'));
    let bracketed = encode(&text, true);
    let body = bracketed
        .strip_prefix(b"\x1b[200~")
        .and_then(|bytes| bytes.strip_suffix(b"\x1b[201~"))
        .unwrap();
    let body = std::str::from_utf8(body).unwrap();
    for output in [plain.as_str(), body] {
        assert!(
            output
                .chars()
                .all(|c| !c.is_control() || matches!(c, '\t' | '\n' | '\r'))
        );
    }
});
