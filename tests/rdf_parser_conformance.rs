//! Hand-authored RDF reader conformance cases (decision D5; review parser#4,
//! parser#5, parser#7, parser#8, parser#9, parser#10, parser#11, parser#12,
//! parser#15).
//!
//! Every case gives a document, the triples the specification requires for it
//! and the section it comes from: RFC 3986 §5.4 (reference resolution, the
//! same for all four formats with a base), the Turtle 1.1 grammar (§6.5:
//! INTEGER / DECIMAL / DOUBLE, PN_PREFIX, comments), RDF 1.1 XML Syntax §7.2
//! (literal property elements, `parseType="Literal"`, empty property
//! elements) and JSON-LD 1.1 (`@base`). The expectations are written from
//! the specifications, not from this implementation's output.
//!
//! The resource tests (single-token caps, tokenizer memory, the complexity
//! of IRI resolution and of JSON-LD / RDFa contexts) measure what one
//! thread allocates with a counting allocator, or compare the run time of an
//! input against one several times larger.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::io::{Cursor, Read};
use std::time::{Duration, Instant};

use gstore::model::{Term, Triple};
use gstore::parser::{jsonld, rdfa, rdfxml, turtle};

// ---- a per-thread counting allocator ----------------------------------------

/// Counts the bytes the current thread holds, and their high-water mark.
/// Thread-local, so tests running in parallel do not disturb each other.
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

const MIB: usize = 1 << 20;

// ---- helpers ------------------------------------------------------------------

fn iri(s: &str) -> Term {
    Term::iri(s)
}

fn lit(v: &str) -> Term {
    Term::plain_literal(v)
}

fn subjects(ts: &[Triple]) -> Vec<String> {
    ts.iter()
        .map(|t| match &t.subject {
            Term::Iri(s) => s.clone(),
            other => format!("{other}"),
        })
        .collect()
}

/// The fastest of three runs of `f`.
fn fastest(mut f: impl FnMut()) -> Duration {
    (0..3)
        .map(|_| {
            let t = Instant::now();
            f();
            t.elapsed()
        })
        .min()
        .unwrap()
}

// ---- RFC 3986 reference resolution in every format (parser#4) ----------------

/// The base of RFC 3986 §5.4 and its examples (§5.4.1 normal, §5.4.2
/// abnormal), as `(reference, resolved IRI)`.
const RFC_BASE: &str = "http://a/b/c/d;p?q";
const RFC_EXAMPLES: &[(&str, &str)] = &[
    ("g:h", "g:h"),
    ("g", "http://a/b/c/g"),
    ("./g", "http://a/b/c/g"),
    ("g/", "http://a/b/c/g/"),
    ("/g", "http://a/g"),
    ("//g", "http://g"),
    ("?y", "http://a/b/c/d;p?y"),
    ("g?y", "http://a/b/c/g?y"),
    ("#s", "http://a/b/c/d;p?q#s"),
    ("g#s", "http://a/b/c/g#s"),
    ("g?y#s", "http://a/b/c/g?y#s"),
    (";x", "http://a/b/c/;x"),
    ("g;x", "http://a/b/c/g;x"),
    ("g;x?y#s", "http://a/b/c/g;x?y#s"),
    ("", "http://a/b/c/d;p?q"),
    (".", "http://a/b/c/"),
    ("./", "http://a/b/c/"),
    ("..", "http://a/b/"),
    ("../", "http://a/b/"),
    ("../g", "http://a/b/g"),
    ("../..", "http://a/"),
    ("../../", "http://a/"),
    ("../../g", "http://a/g"),
    ("../../../g", "http://a/g"),
    ("../../../../g", "http://a/g"),
    ("/./g", "http://a/g"),
    ("/../g", "http://a/g"),
    ("g.", "http://a/b/c/g."),
    (".g", "http://a/b/c/.g"),
    ("g..", "http://a/b/c/g.."),
    ("..g", "http://a/b/c/..g"),
    ("./../g", "http://a/b/g"),
    ("./g/.", "http://a/b/c/g/"),
    ("g/./h", "http://a/b/c/g/h"),
    ("g/../h", "http://a/b/c/h"),
    ("g;x=1/./y", "http://a/b/c/g;x=1/y"),
    ("g;x=1/../y", "http://a/b/c/y"),
    ("g?y/./x", "http://a/b/c/g?y/./x"),
    ("g?y/../x", "http://a/b/c/g?y/../x"),
    ("g#s/./x", "http://a/b/c/g#s/./x"),
    ("g#s/../x", "http://a/b/c/g#s/../x"),
    ("http:g", "http:g"),
];

fn turtle_subject(base: &str, reference: &str) -> String {
    let doc = format!("@base <{base}> .\n<{reference}> <http://ex/p> \"x\" .\n");
    subjects(&turtle::parse_str(&doc).unwrap()).remove(0)
}

fn rdfxml_subject(base: &str, reference: &str) -> String {
    let doc = format!(
        "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" \
         xmlns:ex=\"http://ex/\" xml:base=\"{base}\">\
         <rdf:Description rdf:about=\"{reference}\"><ex:p>x</ex:p></rdf:Description>\
         </rdf:RDF>"
    );
    subjects(&rdfxml::parse_str(&doc).unwrap()).remove(0)
}

fn jsonld_subject(base: &str, reference: &str) -> String {
    let doc = format!(
        "{{\"@context\": {{\"@base\": \"{base}\"}}, \"@id\": \"{reference}\", \
         \"http://ex/p\": \"x\"}}"
    );
    subjects(&jsonld::parse_str(&doc).unwrap()).remove(0)
}

fn rdfa_subject(base: &str, reference: &str) -> String {
    let doc = format!(
        "<html><head><base href=\"{base}\"/></head><body>\
         <div about=\"{reference}\" property=\"http://ex/p\" content=\"x\"></div>\
         </body></html>"
    );
    let ts = rdfa::parse_str(&doc).unwrap();
    let t = ts
        .iter()
        .find(|t| t.predicate == iri("http://ex/p"))
        .unwrap_or_else(|| panic!("RDFa {reference:?}: no ex:p triple in {ts:?}"));
    subjects(std::slice::from_ref(t)).remove(0)
}

/// RFC 3986 §5.4: every example resolves the same, and correctly, in Turtle
/// (`@base`), RDF/XML (`xml:base`), JSON-LD (`@base`) and RDFa
/// (`<base href>`). Before, RDF/XML, JSON-LD and RDFa joined the reference
/// to the base after its last `/` (`../g` → `http://a/b/c/../g`, `//g` →
/// `http://a/b/c///g`, `?y` → `http://a/b/c/?y`).
#[test]
fn relative_references_resolve_per_rfc3986_in_every_format() {
    let mut wrong = Vec::new();
    for &(reference, want) in RFC_EXAMPLES {
        for (format, got) in [
            ("turtle", turtle_subject(RFC_BASE, reference)),
            ("rdfxml", rdfxml_subject(RFC_BASE, reference)),
            ("jsonld", jsonld_subject(RFC_BASE, reference)),
            ("rdfa", rdfa_subject(RFC_BASE, reference)),
        ] {
            if got != want {
                wrong.push(format!("{format}: {reference:?} -> {got:?}, want {want:?}"));
            }
        }
    }
    assert!(wrong.is_empty(), "{} wrong:\n{}", wrong.len(), wrong.join("\n"));
}

/// A base without a path (`http://example.org`) and the review's cases.
#[test]
fn a_base_without_a_path_and_the_review_cases() {
    let cases = [
        ("http://example.org", "foo", "http://example.org/foo"),
        ("http://example.org", "../up", "http://example.org/up"),
        ("http://example.org/a/b", "/abs", "http://example.org/abs"),
        ("http://example.org/a/b", "?q=1", "http://example.org/a/b?q=1"),
        ("http://example.org/a/b", "//other.org/x", "http://other.org/x"),
        ("http://example.org/a/b", "../c", "http://example.org/c"),
        ("http://example.org/a/b", "./d", "http://example.org/a/d"),
    ];
    for (base, reference, want) in cases {
        assert_eq!(turtle_subject(base, reference), want, "turtle {reference}");
        assert_eq!(rdfxml_subject(base, reference), want, "rdfxml {reference}");
        assert_eq!(jsonld_subject(base, reference), want, "jsonld {reference}");
        assert_eq!(rdfa_subject(base, reference), want, "rdfa {reference}");
    }
}

/// A relative base is itself resolved against the base in force: XML Base
/// (`xml:base` on an inner element), JSON-LD 1.1 §4.1.2 (`@base` in a
/// nested context) and Turtle §6.3 (`@base` after `@base`).
#[test]
fn a_relative_base_is_resolved_against_the_outer_base() {
    let ttl = "@base <http://ex.org/a/b/> .\n@base <../c/> .\n<d> <http://ex/p> \"x\" .\n";
    assert_eq!(
        subjects(&turtle::parse_str(ttl).unwrap()),
        ["http://ex.org/a/c/d"]
    );
    let xml = "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" \
               xmlns:ex=\"http://ex/\" xml:base=\"http://ex.org/a/b/\">\
               <rdf:Description xml:base=\"../c/\" rdf:about=\"d\"><ex:p>x</ex:p>\
               </rdf:Description>\
               <rdf:Description rdf:ID=\"frag\"><ex:p>y</ex:p></rdf:Description>\
               </rdf:RDF>";
    assert_eq!(
        subjects(&rdfxml::parse_str(xml).unwrap()),
        ["http://ex.org/a/c/d", "http://ex.org/a/b/#frag"]
    );
    let json = r#"{"@context": {"@base": "http://ex.org/a/b/"},
                   "@id": "s", "http://ex/p": {"@context": {"@base": "../c/"}, "@id": "d"}}"#;
    let ts = jsonld::parse_str(json).unwrap();
    assert_eq!(
        ts,
        [Triple::new(
            iri("http://ex.org/a/b/s"),
            iri("http://ex/p"),
            iri("http://ex.org/a/c/d")
        )]
    );
    let html = "<html><head><base href=\"http://ex.org/a/b/\"/></head><body>\
                <div xml:base=\"../c/\"><span about=\"d\" property=\"http://ex/p\" \
                content=\"x\"></span></div></body></html>";
    let ts = rdfa::parse_str(html).unwrap();
    assert!(
        ts.contains(&Triple::new(
            iri("http://ex.org/a/c/d"),
            iri("http://ex/p"),
            lit("x")
        )),
        "{ts:?}"
    );
}

/// RDF/XML `rdf:datatype` is an IRI reference as well (RDF/XML §7.2.16).
#[test]
fn an_rdfxml_datatype_reference_is_resolved() {
    let xml = "<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" \
               xmlns:ex=\"http://ex/\" xml:base=\"http://ex.org/types/x\">\
               <rdf:Description rdf:about=\"http://ex/s\">\
               <ex:p rdf:datatype=\"#celsius\">21</ex:p></rdf:Description></rdf:RDF>";
    let ts = rdfxml::parse_str(xml).unwrap();
    assert_eq!(
        ts[0].object,
        Term::typed_literal("21", "http://ex.org/types/x#celsius")
    );
}

// ---- resolution is linear in the number of path segments (parser#15) --------

/// Review parser#15: a Turtle IRI with n path segments under `@base` took
/// time quadratic in n (x4 per doubling: 0.9 s at 400 KB, 3.6 s at 800 KB in
/// the test profile). Now an input 8 times larger takes at most about 8
/// times as long (bound: 24, which a quadratic resolver, at 64, misses).
#[test]
fn resolving_a_long_relative_iri_is_linear_in_its_segments() {
    let doc = |segments: usize| {
        format!(
            "@base <http://ex/> .\n<s> <p> <{}x> .\n",
            "a/../b/./".repeat(segments)
        )
    };
    let small = doc(20_000);
    let large = doc(160_000);
    let ts = turtle::parse_str(&large).unwrap();
    assert_eq!(ts[0].object, iri(&format!("http://ex/{}x", "b/".repeat(160_000))));
    let t_small = fastest(|| {
        turtle::parse_str(&small).unwrap();
    });
    let t_large = fastest(|| {
        turtle::parse_str(&large).unwrap();
    });
    let ratio = t_large.as_secs_f64() / t_small.as_secs_f64().max(1e-6);
    assert!(
        ratio < 24.0 || t_large < Duration::from_millis(50),
        "8x the segments took {ratio:.1}x as long ({t_small:?} -> {t_large:?})"
    );
}
