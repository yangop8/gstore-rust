//! OPTIONAL / MINUS: the partial indexes of the right side are bounded
//! (round-3 review of sparqlconf#2 / query#10).
//!
//! The right operand of an OPTIONAL, a MINUS or a join whose shared
//! variables some solutions leave unbound is hashed on the shared
//! variables; a left solution that binds only part of them is looked up in
//! an index on that part, built the first time the part comes up — one
//! index of every right row per part, kept for the whole operator, with no
//! limit. A left side whose solutions each bind a different subset (1,000
//! VALUES rows) then made memory grow as |right| × subsets: 1.6 GB on
//! 10,000 triples for a one-row answer, against 18 MB for the nested loop
//! this replaced. The partial indexes now hold at most a few times the
//! right side's rows together; past that a part is answered by a scan.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use gstore::{Database, QueryResult};

/// Counts the bytes each thread holds, and its peak.
struct Counting;

thread_local! {
    static HELD: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
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

/// Run `f` and return its result with the most bytes this thread held at
/// once during it, beyond what it held before.
fn peak_during<T>(f: impl FnOnce() -> T) -> (T, usize) {
    let base = HELD.with(Cell::get);
    PEAK.with(|p| p.set(base));
    let out = f();
    let peak = PEAK.with(Cell::get);
    (out, (peak - base).max(0) as usize)
}

const VARS: usize = 12;

/// The review's query: 1,000 VALUES rows, each binding a different strict
/// subset of `?v0 … ?v11` (to a node that matches nothing), OPTIONAL a
/// right side that binds them all — or MINUS it.
fn query(op: &str) -> String {
    let vars: Vec<String> = (0..VARS).map(|i| format!("?v{i}")).collect();
    let mut rows = String::new();
    for mask in 1..=1000u32 {
        rows.push_str(" (");
        for i in 0..VARS {
            rows.push_str(if mask & (1 << i) != 0 {
                " <http://ex/none>"
            } else {
                " UNDEF"
            });
        }
        rows.push_str(" )");
    }
    let binds: String = (0..VARS).map(|i| format!(" BIND(?s AS ?v{i})")).collect();
    format!(
        "SELECT (COUNT(*) AS ?c) {{ VALUES ({}) {{{rows} }} {op} {{ ?s <http://ex/p> ?o{binds} }} }}",
        vars.join(" ")
    )
}

fn data(triples: usize) -> Database {
    let nt: String = (0..triples)
        .map(|i| format!("<http://ex/s{i}> <http://ex/p> <http://ex/o{i}> .\n"))
        .collect();
    Database::build_from_str("partial", &nt).unwrap()
}

fn count(db: &mut Database, q: &str) -> String {
    match db.query(q).unwrap() {
        QueryResult::Select(rs) => rs.rows[0][0].clone().unwrap(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn many_partial_binding_patterns_keep_memory_linear_in_the_right_side() {
    for op in ["OPTIONAL", "MINUS"] {
        let q = query(op);
        let mut peaks = Vec::new();
        for triples in [2000, 4000] {
            let mut db = data(triples);
            let (c, peak) = peak_during(|| count(&mut db, &q));
            // Every left row is kept: nothing matches `<none>`.
            assert!(c.contains("1000"), "{op}: {c}");
            peaks.push(peak);
        }
        // About 160 bytes per right row and binding pattern before: 320 MB
        // and 640 MB here. Now a few times the rows.
        assert!(
            peaks[1] < 48 << 20,
            "{op}: peak {} bytes on 4,000 triples ({} on 2,000)",
            peaks[1],
            peaks[0]
        );
        assert!(
            peaks[1] < 3 * peaks[0] + (1 << 20),
            "{op}: twice the right side took {} -> {} bytes",
            peaks[0],
            peaks[1]
        );
    }
}
