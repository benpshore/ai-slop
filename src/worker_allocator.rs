//! Temporary, opt-in evidence for allocation failures in disposable workers.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);
static FAILURES: AtomicUsize = AtomicUsize::new(0);
static LAST_BYTES: AtomicUsize = AtomicUsize::new(0);

struct ObservedSystem;

#[global_allocator]
static ALLOCATOR: ObservedSystem = ObservedSystem;

fn observe(pointer: *mut u8, bytes: usize) -> *mut u8 {
    if pointer.is_null() && ENABLED.load(Ordering::Relaxed) {
        FAILURES.fetch_add(1, Ordering::Relaxed);
        LAST_BYTES.store(bytes, Ordering::Relaxed);
    }
    pointer
}

// SAFETY: every allocation and deallocation is forwarded unchanged to System.
unsafe impl GlobalAlloc for ObservedSystem {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller supplies a valid allocation layout.
        observe(unsafe { System.alloc(layout) }, layout.size())
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller supplies a valid allocation layout.
        observe(unsafe { System.alloc_zeroed(layout) }, layout.size())
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: the pointer and layout came from this same System allocator.
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, bytes: usize) -> *mut u8 {
        // SAFETY: the caller supplies the original allocation and valid new size.
        observe(unsafe { System.realloc(pointer, layout, bytes) }, bytes)
    }
}

pub(super) fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

pub(super) fn evidence(label: &str) {
    if ENABLED.load(Ordering::Relaxed) {
        eprintln!(
            "pressure-allocation {label}: failures={} last_failed_bytes={}",
            FAILURES.load(Ordering::Relaxed),
            LAST_BYTES.load(Ordering::Relaxed)
        );
    }
}
