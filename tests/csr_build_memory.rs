//! review analytics#6: the CSR of an in-memory database is built by reading
//! its store in place, predicate by predicate, copying only entity→entity
//! pairs. The literal triples the CSR skips are never copied, so the build
//! needs memory in proportion to the entity edges, not to the whole store.
//! Before the fix, `Database::csr` went through `CsrGraph::build`, which
//! first collects every triple of the store (`iter_all`, 12 bytes each).
//!
//! The heap is measured with a counting global allocator. This binary holds
//! one test, so nothing else allocates during the measurement.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use gstore::Database;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Counting;

fn grew(by: usize) {
    let now = CURRENT.fetch_add(by, Ordering::SeqCst) + by;
    PEAK.fetch_max(now, Ordering::SeqCst);
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        CURRENT.fetch_sub(layout.size(), Ordering::SeqCst);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                grew(new_size - layout.size());
            } else {
                CURRENT.fetch_sub(layout.size() - new_size, Ordering::SeqCst);
            }
        }
        p
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

#[test]
fn an_in_memory_csr_build_does_not_copy_the_literal_triples() {
    const ENTITIES: usize = 2_000;
    const LITERALS: usize = 200_000;
    // A ring of entity edges, and many literal triples on the same subjects.
    let mut text = String::new();
    for i in 0..ENTITIES {
        text.push_str(&format!(
            "<http://ex/n{i}> <http://ex/next> <http://ex/n{}> .\n",
            (i + 1) % ENTITIES
        ));
    }
    for i in 0..LITERALS {
        text.push_str(&format!(
            "<http://ex/n{}> <http://ex/value> \"value {i}\" .\n",
            i % ENTITIES
        ));
    }
    let db = Database::build_from_str("csr-memory", &text).unwrap();
    drop(text);

    let base = CURRENT.load(Ordering::SeqCst);
    PEAK.store(base, Ordering::SeqCst);
    let graph = db.csr().unwrap();
    let used = PEAK.load(Ordering::SeqCst).saturating_sub(base);

    assert_eq!(graph.num_edges(), ENTITIES as u64);
    // One copy of every triple, as `iter_all` made it (12 bytes each).
    let all_triples = (ENTITIES + LITERALS) * 12;
    assert!(
        used * 4 < all_triples,
        "the CSR build peaked at {used} bytes above its start; a copy of every triple \
         is {all_triples} bytes"
    );
}
