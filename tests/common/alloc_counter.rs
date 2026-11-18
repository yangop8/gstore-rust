//! A counting global allocator, for tests that bound memory or work by the
//! bytes a call allocates instead of by its elapsed time (review conc#16: a
//! wall-clock bound fails on a loaded machine and passes a regression on a
//! fast one; byte counts are the same on every machine).
//!
//! Per thread, it counts the bytes held now, their peak, and the bytes
//! allocated in all. It has to be the binary's global allocator, so it is
//! not part of `mod common`: a test binary includes it on its own with
//! `#[path = "common/alloc_counter.rs"] mod alloc_counter;`.

#![allow(dead_code)] // each test target uses a subset

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

/// The allocator: the system's, counting.
pub struct Counting;

thread_local! {
    static HELD: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
    static TOTAL: Cell<usize> = const { Cell::new(0) };
}

fn note(delta: isize) {
    let _ = HELD.try_with(|h| {
        let now = h.get() + delta;
        h.set(now);
        let _ = PEAK.try_with(|p| {
            if now > p.get() {
                p.set(now)
            }
        });
    });
    if delta > 0 {
        let _ = TOTAL.try_with(|t| t.set(t.get().saturating_add(delta as usize)));
    }
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            note(layout.size() as isize);
        }
        p
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        note(-(layout.size() as isize));
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            note(layout.size() as isize);
        }
        p
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            note(new_size as isize - layout.size() as isize);
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Run `f` on this thread and return its result with the most bytes this
/// thread held at once during it, beyond what it held before.
pub fn peak_during<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = HELD.with(Cell::get);
    PEAK.with(|p| p.set(base));
    let out = f();
    let peak = PEAK.with(Cell::get);
    (out, (peak - base).max(0) as usize)
}

/// Run `f` on this thread and return its result with the bytes this thread
/// allocated during it, in all (a measure of work for code that builds
/// strings and collections).
pub fn allocated_during<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let before = TOTAL.with(Cell::get);
    let out = f();
    (out, TOTAL.with(Cell::get) - before)
}
