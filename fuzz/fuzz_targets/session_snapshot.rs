//! Any bytes in `sessions/default.json` must never panic the core's snapshot parse and check.

#![no_main]

use libfuzzer_sys::fuzz_target;
use pinfold_core::snapshot;

fuzz_target!(|data: &[u8]| {
    if let Ok(snapshot) = snapshot::parse(data) {
        // What passed the check rebuilds, so a restore spawns only from a valid layout.
        assert!(pinfold_mux::Session::from_tree(&snapshot.tree(), 0).is_ok());
    }
});
