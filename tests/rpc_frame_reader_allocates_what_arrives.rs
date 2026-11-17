//! review robust#15 / sec#19 (merged into cluster-nlqa#9): the experimental
//! RPC frame reader (`gstore::rpc::read_message`, used by `gnode` and every
//! cluster connection) allocated the whole announced frame — up to
//! `MAX_FRAME` = 256 MiB — from an unauthenticated 4-byte length prefix,
//! before a single payload byte arrived. A peer could pin 256 MiB per
//! connection with four bytes. The buffer must grow with the bytes received.
//!
//! The largest single allocation is measured with a counting global
//! allocator; this file holds one test so nothing else allocates in parallel.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{Cursor, ErrorKind};
use std::sync::atomic::{AtomicUsize, Ordering};

use gstore::rpc::{read_message, write_message, MAX_FRAME};

/// The largest block requested from the allocator since the last reset.
static LARGEST: AtomicUsize = AtomicUsize::new(0);

struct Largest;

unsafe impl GlobalAlloc for Largest {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::SeqCst);
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::SeqCst);
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        LARGEST.fetch_max(new_size, Ordering::SeqCst);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: Largest = Largest;

#[test]
fn a_forged_length_prefix_does_not_allocate_the_announced_frame() {
    // A prefix announcing the largest legal frame, three payload bytes, EOF.
    let mut forged = (MAX_FRAME as u32).to_be_bytes().to_vec();
    forged.extend_from_slice(b"abc");
    let mut input = Cursor::new(forged);

    LARGEST.store(0, Ordering::SeqCst);
    let err = read_message(&mut input).expect_err("the frame is truncated");
    let largest = LARGEST.load(Ordering::SeqCst);

    assert_eq!(err.kind(), ErrorKind::UnexpectedEof, "{err}");
    assert!(
        largest < 1 << 20,
        "reading 3 payload bytes of an announced {MAX_FRAME}-byte frame allocated a \
         {largest}-byte block"
    );

    // Legitimate frames, large and small, still round-trip byte for byte.
    for len in [0usize, 1, 4096, 3 << 20] {
        let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let mut wire = Vec::new();
        write_message(&mut wire, &payload).unwrap();
        assert_eq!(read_message(&mut Cursor::new(wire)).unwrap(), payload);
    }
}
