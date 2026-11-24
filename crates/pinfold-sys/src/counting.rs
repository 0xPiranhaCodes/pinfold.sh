//! An allocator that counts allocations, for tests that check a path allocates nothing
//! (ADR-0053). It passes every call to the system allocator; only a counter is added.
//!
//! # Examples
//!
//! ```
//! // A test binary installs it with
//! // `#[global_allocator] static COUNTING: pinfold_sys::counting::Counting = Counting;`.
//! let before = pinfold_sys::counting::allocations();
//! assert!(pinfold_sys::counting::allocations() >= before);
//! ```

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

/// The counting allocator.
pub struct Counting;

/// Allocations and reallocations made through [`Counting`] since the process started.
#[must_use]
pub fn allocations() -> u64 {
    ALLOCATIONS.load(Ordering::Relaxed)
}

// SAFETY: every call is passed to `System` unchanged, so its contract holds.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the caller upholds `alloc`'s contract, which is passed on.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `dealloc`'s contract, which is passed on.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the caller upholds `realloc`'s contract, which is passed on.
        unsafe { System.realloc(ptr, layout, size) }
    }
}
