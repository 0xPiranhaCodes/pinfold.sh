//! The tools' key decoder never panics, and gives the same keys however the bytes are split.

#![no_main]

use libfuzzer_sys::fuzz_target;
use pinfold_tools::keys::{Key, Keys};

fn drain(keys: &mut Keys, quiet: bool, into: &mut Vec<Key>) {
    while let Some(key) = keys.next(quiet) {
        into.push(key);
    }
}

fuzz_target!(|data: &[u8]| {
    let (mut whole, mut split) = (Vec::new(), Vec::new());
    let mut keys = Keys::default();
    keys.push(data);
    drain(&mut keys, false, &mut whole);
    drain(&mut keys, true, &mut whole);
    let mut keys = Keys::default();
    for byte in data {
        keys.push(&[*byte]);
        drain(&mut keys, false, &mut split);
    }
    drain(&mut keys, true, &mut split);
    assert_eq!(whole, split);
    assert_eq!(keys.next(true), None);
});
