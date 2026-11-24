//! Recording a call allocates nothing once the ring and its text ring exist (ADR-0053): a
//! counting allocator, in a test binary of its own so no other test allocates meanwhile.

use pinfold_api::Log;
use pinfold_api::audit::{Call, Detail, Verdict};
use pinfold_sys::counting::{Counting, allocations};

#[global_allocator]
static COUNTING: Counting = Counting;

fn call(n: u64) -> Call<'static> {
    let (verdict, reason, detail) = match n % 4 {
        0 => (Verdict::Ok, "allowed", Detail::None),
        1 => (Verdict::Ok, "allowed", Detail::Bytes(n)),
        2 => (Verdict::Refused, "no such pane", Detail::Grant(n)),
        _ => (
            Verdict::Refused,
            "cannot start pinfold-window: not in the table",
            Detail::None,
        ),
    };
    Call {
        at_ms: 1_763_000_000_000 + n,
        caller: Some(n % 8),
        terminal: true,
        method: "pane.list",
        target: Some(n),
        scope: "terminal",
        verdict,
        reason,
        detail,
    }
}

#[test]
fn a_recorded_call_allocates_nothing_after_the_first() {
    let mut log = Log::new();
    // The first calls allocate the ring and the text ring, once.
    for n in 0..4 {
        log.record(&call(n)).unwrap();
    }
    let before = allocations();
    for n in 4..10_000 {
        log.record(&call(n)).unwrap();
    }
    assert_eq!(allocations() - before, 0);
    // The allocator is the one counting.
    drop(Box::new(before));
    assert!(allocations() > before);
    log.verify().unwrap();
}
