//! An RDF/XML `rdf:parseType="Literal"` value is bounded while its
//! canonical form is written (round-3 review; RDF 1.1 XML Syntax §7.2.17).
//!
//! Exclusive XML Canonicalization repeats each namespace a top-level
//! element of the literal uses on that element, so the value can be far
//! longer than its markup: a namespace declared once with a 1 MiB IRI and
//! used by N sibling elements costs N MiB. The document's expansion budget
//! (`TermBudget`) was checked only once the whole literal string existed:
//! 400 siblings built a 400 MB string before the document was refused, and
//! 4000 siblings (a 1 MB upload) asked for a 4 GiB allocation and aborted
//! the process. The canonical form now stops — with the same "expands to
//! more than … bytes" error — as soon as it passes what is left of the
//! budget.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use gstore::parser::rdfxml;

/// Counts the bytes each thread holds, and its peak.
struct Counting;

thread_local! {
    static HELD: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

fn note(delta: isize) {
    // `try_with`: the thread's slots may be gone while it exits.
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

/// A document declaring the namespace `a:` with an IRI of `iri_len` bytes
/// on `rdf:RDF`, with one XML literal holding `siblings` `<a:x/>`.
fn document(iri_len: usize, siblings: usize) -> String {
    let iri = format!("http://example.org/{}", "n".repeat(iri_len));
    format!(
        "<?xml version=\"1.0\"?>\n\
         <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" \
         xmlns:ex=\"http://example.org/\" xmlns:a=\"{iri}\">\n\
         <rdf:Description rdf:about=\"http://example.org/s\">\n\
         <ex:p rdf:parseType=\"Literal\">{}</ex:p>\n\
         </rdf:Description>\n</rdf:RDF>\n",
        "<a:x/>".repeat(siblings)
    )
}

/// The budget a refusal names ("… more than N bytes …").
fn budget_in(msg: &str) -> usize {
    msg.split("more than ")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no budget in {msg:?}"))
}

#[test]
fn a_literal_repeating_a_long_namespace_is_refused_within_its_budget() {
    // 400 siblings under a 1 MiB namespace: a 400 MiB canonical form.
    let doc = document(1 << 20, 400);
    let (res, peak) = peak_during(|| rdfxml::parse_str(&doc));
    let err = res.expect_err("the literal is past the document's expansion budget");
    let msg = err.to_string();
    assert!(msg.contains("expands to more than"), "{msg}");
    let budget = budget_in(&msg);
    // The canonical form stops once it passes the budget: the string's
    // capacity (at most twice its length) and the parsed document stay
    // within three budgets. It used to reach the whole 400 MiB first.
    assert!(
        peak < 3 * budget,
        "peak {peak} bytes for a {}-byte document with a budget of {budget} bytes",
        doc.len()
    );
}

#[test]
fn a_literal_within_its_budget_keeps_its_canonical_form() {
    // A few siblings: the namespace is declared on each, in full.
    let doc = document(64, 3);
    let triples = rdfxml::parse_str(&doc).expect("parse");
    assert_eq!(triples.len(), 1);
    let iri = format!("http://example.org/{}", "n".repeat(64));
    let one = format!("<a:x xmlns:a=\"{iri}\"></a:x>");
    match &triples[0].object {
        gstore::model::Term::Literal {
            value, datatype, ..
        } => {
            assert_eq!(value, &one.repeat(3));
            assert_eq!(
                datatype.as_deref(),
                Some("http://www.w3.org/1999/02/22-rdf-syntax-ns#XMLLiteral")
            );
        }
        other => panic!("{other:?}"),
    }
}
