//! Data test: a graph function's output bound
//! ([`GFunCtx::with_max_output_bytes`], `conf.ini` `[query]
//! gfun_max_output_bytes`) limits the memory a call uses, not only the size
//! of what it returns. `JaccardSimilarity` and `PPR` write up to `retNum`
//! entries per distinct source — quadratic in the vertex count — so a call
//! must fail as soon as its output passes the bound, not after building all
//! of it.
//!
//! The peak is measured with a counting global allocator, so this file holds
//! a single test: no other test may allocate concurrently.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use gstore::analytics::csr::PredFilter;
use gstore::analytics::gfun::{self, GArgs, GFnKind, GFunCtx};
use gstore::error::GStoreError;
use gstore::Database;

/// The system allocator, counting the bytes live now and the peak since
/// [`reset_peak`].
struct Counting;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(by: usize) {
    let now = CURRENT.fetch_add(by, Ordering::SeqCst) + by;
    PEAK.fetch_max(now, Ordering::SeqCst);
}

// SAFETY: every method forwards to `System` with the caller's arguments; the
// counters do not affect allocation.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc(layout);
        if !p.is_null() {
            grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        CURRENT.fetch_sub(layout.size(), Ordering::SeqCst);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = System.realloc(ptr, layout, new_size);
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

/// Start a new peak measurement; returns the bytes live now.
fn reset_peak() -> usize {
    let now = CURRENT.load(Ordering::SeqCst);
    PEAK.store(now, Ordering::SeqCst);
    now
}

/// A star: `<l_i> <p> <hub>` for `leaves` leaves (every pair of leaves is
/// two hops apart), and `<hub> <p> <l_i>` too when `both_ways` (so random
/// walks from a leaf reach every other leaf).
fn star(leaves: usize, both_ways: bool) -> String {
    (0..leaves)
        .map(|i| {
            let back = if both_ways {
                format!("<http://x/hub> <http://x/p> <http://x/l{i}> .\n")
            } else {
                String::new()
            };
            format!("<http://x/l{i}> <http://x/p> <http://x/hub> .\n{back}")
        })
        .collect()
}

#[test]
fn quadratic_graph_functions_stop_at_the_output_bound() {
    const BOUND: usize = 64 * 1024;
    // The most a call may allocate past the bound: one more entry, the
    // per-source working sets and the result's copy — far below the full
    // outputs (Jaccard ≈ 1500² entries ≈ 70 MB, PPR ≈ 400 × 400 entries
    // ≈ 6 MB).
    const SLACK: usize = 2 * 1024 * 1024;
    let cases: [(GFnKind, usize, i64); 2] = [
        (GFnKind::JaccardSimilarity, 1500, 100_000),
        (GFnKind::Ppr, 400, 100_000),
    ];
    for (kind, leaves, ret_num) in cases {
        let both_ways = kind == GFnKind::Ppr;
        let db = Database::build_from_str("gfun-bound", &star(leaves, both_ways)).unwrap();
        let csr = db.csr().unwrap();
        let ctx = GFunCtx::new(&csr, db.dict()).with_max_output_bytes(BOUND);
        let srcs = (0..leaves)
            .map(|i| gfun::iri_node(&csr, db.dict(), &format!("<http://x/l{i}>")))
            .collect();
        let args = GArgs {
            srcs,
            directed: true,
            k: Some(if kind == GFnKind::Ppr { -1 } else { 2 }),
            ret_num: Some(ret_num),
            preds: PredFilter::resolve::<&str>(&[], &[], db.dict()),
            ..GArgs::default()
        };
        let base = reset_peak();
        let r = gfun::eval(kind, &ctx, &args);
        let used = PEAK.load(Ordering::SeqCst) - base;
        match r {
            Err(GStoreError::Query(m)) => assert!(m.contains("bytes of output"), "{kind}: {m}"),
            Err(e) => panic!("{kind}: expected the output bound, got {e}"),
            Ok(v) => panic!(
                "{kind}: expected the output bound, got {} bytes",
                format!("{v:?}").len()
            ),
        }
        assert!(
            used < BOUND + SLACK,
            "{kind}: allocated {used} bytes before failing at a {BOUND}-byte bound"
        );
    }
}
