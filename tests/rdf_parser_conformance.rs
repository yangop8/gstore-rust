//! Hand-authored RDF reader conformance cases (decision D5; review parser#4,
//! parser#5, parser#7, parser#8, parser#9, parser#10, parser#11, parser#12,
//! parser#14, parser#15).
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
//! input against one several times larger; the blank-graph test counts the
//! graphs of a generated million-label document kept apart by a false
//! positive.

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
    assert!(
        wrong.is_empty(),
        "{} wrong:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}

/// A base without a path (`http://example.org`) and the review's cases.
#[test]
fn a_base_without_a_path_and_the_review_cases() {
    let cases = [
        ("http://example.org", "foo", "http://example.org/foo"),
        ("http://example.org", "../up", "http://example.org/up"),
        ("http://example.org/a/b", "/abs", "http://example.org/abs"),
        (
            "http://example.org/a/b",
            "?q=1",
            "http://example.org/a/b?q=1",
        ),
        (
            "http://example.org/a/b",
            "//other.org/x",
            "http://other.org/x",
        ),
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
    assert_eq!(
        ts[0].object,
        iri(&format!("http://ex/{}x", "b/".repeat(160_000)))
    );
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

// ---- RDF/XML literals keep their data (parser#9) ------------------------------

const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

/// An RDF/XML document with the `rdf:` and `ex:` namespaces (plus `extra`
/// declarations on `rdf:RDF`) and one `rdf:Description` of `ex:s`.
fn rdfxml_doc(extra: &str, body: &str) -> String {
    format!(
        "<rdf:RDF xmlns:rdf=\"{RDF}\" xmlns:ex=\"http://ex/\" {extra}>\
         <rdf:Description rdf:about=\"http://ex/s\">{body}</rdf:Description></rdf:RDF>"
    )
}

fn object_of(ts: &[Triple], p: &str) -> Term {
    ts.iter()
        .find(|t| t.predicate == iri(p))
        .unwrap_or_else(|| panic!("no {p} in {ts:?}"))
        .object
        .clone()
}

/// RDF/XML §7.2.16 (literalPropertyElt): the literal is the element's text,
/// whitespace included. Before, it was trimmed ("  two  spaces  " became
/// "two  spaces", a whitespace-only value "").
#[test]
fn an_rdfxml_literal_keeps_its_whitespace() {
    let cases = [
        ("  two  spaces  ", "  two  spaces  "),
        ("\nline1\n  line2\n", "\nline1\n  line2\n"),
        ("   ", "   "),
        ("a<!-- note -->b", "ab"),
        ("<![CDATA[ <raw> ]]>", " <raw> "),
        ("x &amp; y", "x & y"),
    ];
    for (content, want) in cases {
        let doc = rdfxml_doc("", &format!("<ex:code>{content}</ex:code>"));
        let ts = rdfxml::parse_str(&doc).unwrap();
        assert_eq!(object_of(&ts, "http://ex/code"), lit(want), "{content:?}");
    }
    // With a language and a datatype.
    let doc = rdfxml_doc(
        "",
        "<ex:a xml:lang=\"en\"> hi </ex:a>\
         <ex:b rdf:datatype=\"http://www.w3.org/2001/XMLSchema#string\"> x </ex:b>",
    );
    let ts = rdfxml::parse_str(&doc).unwrap();
    assert_eq!(
        object_of(&ts, "http://ex/a"),
        Term::Literal {
            value: " hi ".into(),
            datatype: None,
            lang: Some("en".into())
        }
    );
    assert_eq!(
        object_of(&ts, "http://ex/b"),
        Term::typed_literal(" x ", "http://www.w3.org/2001/XMLSchema#string")
    );
}

/// RDF/XML §7.2.17 (parseTypeLiteralPropertyElt): the value is the content,
/// markup included, as exclusive canonical XML with comments: each element
/// declares the namespaces it visibly uses, declarations before attributes,
/// attributes by namespace IRI then local name, `<br/>` as `<br></br>`,
/// text escaped. Before, only the text children were kept (" text").
#[test]
fn an_rdfxml_xml_literal_keeps_its_markup() {
    let xml_literal = |v: &str| Term::typed_literal(v, format!("{RDF}XMLLiteral"));
    let doc = rdfxml_doc(
        "xmlns:h=\"http://www.w3.org/1999/xhtml\"",
        "<ex:xml rdf:parseType=\"Literal\"><h:b h:id=\"i\" class='x'>bold &amp; &lt;b&gt;</h:b> \
         text<!-- c --><h:br/></ex:xml>",
    );
    let ts = rdfxml::parse_str(&doc).unwrap();
    assert_eq!(
        object_of(&ts, "http://ex/xml"),
        xml_literal(
            "<h:b xmlns:h=\"http://www.w3.org/1999/xhtml\" class=\"x\" h:id=\"i\">bold &amp; \
             &lt;b&gt;</h:b> text<!-- c --><h:br xmlns:h=\"http://www.w3.org/1999/xhtml\"></h:br>"
        )
    );
    // The default namespace in scope is declared on the outermost element
    // that uses it, not again inside; one declared inside the literal stays
    // where it is used.
    let doc = rdfxml_doc(
        "xmlns=\"http://ex/d/\"",
        "<ex:xml rdf:parseType=\"Literal\"><p>a <i>b</i></p>\
         <q xmlns:z=\"http://z/\" xmlns:unused=\"http://u/\"><z:r a=\"1&#10;2\"/></q></ex:xml>",
    );
    let ts = rdfxml::parse_str(&doc).unwrap();
    assert_eq!(
        object_of(&ts, "http://ex/xml"),
        xml_literal(
            "<p xmlns=\"http://ex/d/\">a <i>b</i></p><q xmlns=\"http://ex/d/\">\
             <z:r xmlns:z=\"http://z/\" a=\"1&#xA;2\"></z:r></q>"
        )
    );
    // Text only: the text, escaped as canonical XML.
    let doc = rdfxml_doc(
        "",
        "<ex:xml rdf:parseType=\"Literal\">a &lt; b &amp; c</ex:xml>",
    );
    let ts = rdfxml::parse_str(&doc).unwrap();
    assert_eq!(
        object_of(&ts, "http://ex/xml"),
        xml_literal("a &lt; b &amp; c")
    );
}

/// Exclusive canonicalization (XML-EXC-C14N §3, the namespaces an element
/// visibly uses): a binding the nearest enclosing output element already
/// rendered with the same value is not declared again; a different value,
/// or an inner element back in no namespace (`xmlns=""`), is.
#[test]
fn an_rdfxml_xml_literal_redeclares_only_changed_namespaces() {
    let xml_literal = |v: &str| Term::typed_literal(v, format!("{RDF}XMLLiteral"));
    let doc = rdfxml_doc(
        "xmlns:h=\"http://h/\"",
        "<ex:xml rdf:parseType=\"Literal\"><h:a><h:b xmlns:h=\"http://h/\">\
         <h:c xmlns:h=\"http://h2/\"><h:d/></h:c></h:b></h:a>\
         <a xmlns=\"http://d/\"><b xmlns=\"\"><c/></b></a></ex:xml>",
    );
    let ts = rdfxml::parse_str(&doc).unwrap();
    assert_eq!(
        object_of(&ts, "http://ex/xml"),
        xml_literal(
            "<h:a xmlns:h=\"http://h/\"><h:b><h:c xmlns:h=\"http://h2/\"><h:d></h:d></h:c>\
             </h:b></h:a><a xmlns=\"http://d/\"><b xmlns=\"\"><c></c></b></a>"
        )
    );
}

/// XML Namespaces §6.1 (scoping): a declaration applies to the element
/// that carries it and its content, an inner one hides an outer one there,
/// and siblings keep the outer binding. (Found in review: a declaration on
/// any element but `rdf:RDF` failed the document, "undefined namespace
/// prefix 'xmlns:'", so did a node element as the document element, RDF/XML
/// §7.2.11.)
#[test]
fn an_rdfxml_namespace_declaration_is_scoped_to_its_element() {
    let doc = format!(
        "<ex:T xmlns:rdf=\"{RDF}\" xmlns:ex=\"http://ex/\" rdf:about=\"http://ex/s0\">\
         <ex:p xmlns:ex=\"http://inner/\" rdf:resource=\"http://ex/o\"/></ex:T>"
    );
    assert_eq!(
        rdfxml::parse_str(&doc).unwrap(),
        [
            Triple::new(
                iri("http://ex/s0"),
                iri(&format!("{RDF}type")),
                iri("http://ex/T")
            ),
            Triple::new(
                iri("http://ex/s0"),
                iri("http://inner/p"),
                iri("http://ex/o")
            ),
        ]
    );
    let doc = format!(
        "<rdf:RDF xmlns:rdf=\"{RDF}\" xmlns:ex=\"http://ex/\">\
         <rdf:Description rdf:about=\"http://ex/s1\" xmlns:ex=\"http://inner/\">\
           <ex:p>a</ex:p><ex:q xmlns:ex=\"http://innermost/\">b</ex:q><ex:r>c</ex:r>\
         </rdf:Description>\
         <rdf:Description rdf:about=\"http://ex/s2\"><ex:p>d</ex:p></rdf:Description></rdf:RDF>"
    );
    let ts = rdfxml::parse_str(&doc).unwrap();
    for (s, p, o) in [
        ("http://ex/s1", "http://inner/p", "a"),
        ("http://ex/s1", "http://innermost/q", "b"),
        ("http://ex/s1", "http://inner/r", "c"),
        ("http://ex/s2", "http://ex/p", "d"),
    ] {
        assert!(
            ts.contains(&Triple::new(iri(s), iri(p), lit(o))),
            "{s} {p} {o:?} missing from {ts:?}"
        );
    }
    assert_eq!(ts.len(), 4, "{ts:?}");
}

/// The pattern of review parser#7 in the RDF/XML reader (found in review):
/// every node and property element copied every namespace binding declared
/// above it (40000 elements: 0.12 s under 10 declarations, 3.8 s under
/// 1000). The bindings are now shared layers: 100 times the declarations
/// cost about the same (bound: 4).
#[test]
fn rdfxml_elements_share_the_namespace_bindings() {
    let elements = 20000;
    let doc = |declarations: usize| {
        let decl: String = (0..declarations)
            .map(|i| format!(" xmlns:n{i}=\"http://ex/n{i}/\""))
            .collect();
        let body: String = (0..elements)
            .map(|i| {
                format!(
                    "<rdf:Description rdf:about=\"http://ex/s{i}\"><ex:p>x</ex:p></rdf:Description>"
                )
            })
            .collect();
        format!("<rdf:RDF xmlns:rdf=\"{RDF}\" xmlns:ex=\"http://ex/\"{decl}>{body}</rdf:RDF>")
    };
    let small = doc(10);
    let large = doc(1000);
    assert_eq!(rdfxml::parse_str(&large).unwrap().len(), elements);
    let t_small = fastest(|| {
        rdfxml::parse_str(&small).unwrap();
    });
    let t_large = fastest(|| {
        rdfxml::parse_str(&large).unwrap();
    });
    let ratio = t_large.as_secs_f64() / t_small.as_secs_f64().max(1e-6);
    assert!(
        ratio < 4.0 || t_large < Duration::from_millis(30),
        "100x the namespace declarations took {ratio:.1}x as long ({t_small:?} -> {t_large:?})"
    );
}

/// RDF/XML §7.2.21 (emptyPropertyElt): with property attributes the object
/// is `rdf:resource`, `rdf:nodeID` or a fresh blank node, and the property
/// attributes (and `rdf:type`) describe it; without any it is the literal
/// "" (with the `xml:lang` in scope). Before, `<ex:empty ex:name="n"/>`
/// gave the literal "" and `<ex:rel rdf:resource=… ex:name="n"/>` lost
/// `ex:name`.
#[test]
fn an_empty_rdfxml_property_element_describes_its_object() {
    let doc = rdfxml_doc(
        "",
        "<ex:empty ex:name=\"n\" rdf:type=\"http://ex/T\"/>\
         <ex:rel rdf:resource=\"http://ex/o\" ex:name=\"m\"/>\
         <ex:node rdf:nodeID=\"k\" ex:name=\"k1\"/>\
         <ex:nothing/>\
         <ex:tagged xml:lang=\"fr\"/>",
    );
    let ts = rdfxml::parse_str(&doc).unwrap();
    let s = iri("http://ex/s");
    let name = iri("http://ex/name");
    let b = object_of(&ts, "http://ex/empty");
    assert!(matches!(b, Term::Blank(_)), "{b:?}");
    let k = object_of(&ts, "http://ex/node");
    assert!(matches!(k, Term::Blank(_)), "{k:?}");
    let want = [
        Triple::new(s.clone(), iri("http://ex/empty"), b.clone()),
        Triple::new(b.clone(), iri(&format!("{RDF}type")), iri("http://ex/T")),
        Triple::new(b, name.clone(), lit("n")),
        Triple::new(s.clone(), iri("http://ex/rel"), iri("http://ex/o")),
        Triple::new(iri("http://ex/o"), name.clone(), lit("m")),
        Triple::new(s.clone(), iri("http://ex/node"), k.clone()),
        Triple::new(k, name, lit("k1")),
        Triple::new(s.clone(), iri("http://ex/nothing"), lit("")),
        Triple::new(
            s,
            iri("http://ex/tagged"),
            Term::Literal {
                value: String::new(),
                datatype: None,
                lang: Some("fr".into()),
            },
        ),
    ];
    let mut got = ts.clone();
    let mut want = want.to_vec();
    got.sort_by_key(|t| format!("{t:?}"));
    want.sort_by_key(|t| format!("{t:?}"));
    assert_eq!(got, want);
}

// ---- JSON-LD contexts and RDFa prefix maps are shared (parser#7) -------------

fn jsonld_with_terms(terms: usize, nodes: usize) -> String {
    let defs: Vec<String> = (0..terms)
        .map(|i| format!("\"t{i}\": \"http://ex/t{i}\""))
        .collect();
    let graph: Vec<String> = (0..nodes)
        .map(|i| format!("{{\"@id\": \"http://ex/n{i}\", \"t0\": \"v\"}}"))
        .collect();
    format!(
        "{{\"@context\": {{{}}}, \"@graph\": [{}]}}",
        defs.join(", "),
        graph.join(", ")
    )
}

/// Review parser#7: every JSON-LD node object copied the whole active
/// context (O(nodes x terms): 10.9 s for 20000 `{}` under a 4000-term
/// context). With the context shared, 16 times the terms cost about the
/// same, not 16 times as much (bound: 4).
#[test]
fn jsonld_node_objects_share_the_active_context() {
    let nodes = 5000;
    let small = jsonld_with_terms(250, nodes);
    let large = jsonld_with_terms(4000, nodes);
    assert_eq!(jsonld::parse_str(&large).unwrap().len(), nodes);
    let t_small = fastest(|| {
        jsonld::parse_str(&small).unwrap();
    });
    let t_large = fastest(|| {
        jsonld::parse_str(&large).unwrap();
    });
    let ratio = t_large.as_secs_f64() / t_small.as_secs_f64().max(1e-6);
    assert!(
        ratio < 4.0 || t_large < Duration::from_millis(30),
        "16x the context terms took {ratio:.1}x as long ({t_small:?} -> {t_large:?})"
    );
}

/// A node object's own `@context` applies to it and to what it contains,
/// not to its siblings (JSON-LD 1.1 §4.1: contexts are scoped to the object
/// that carries them).
#[test]
fn a_jsonld_node_context_is_scoped_to_its_node() {
    let doc = r#"{"@context": {"ex": "http://ex/", "name": "http://ex/name"},
        "@graph": [
          {"@id": "ex:a", "@context": {"name": "http://other/name"}, "name": "A",
           "ex:child": {"@id": "ex:c", "name": "C"}},
          {"@id": "ex:b", "name": "B"}
        ]}"#;
    let mut got = jsonld::parse_str(doc).unwrap();
    got.sort_by_key(|t| format!("{t:?}"));
    let mut want = vec![
        Triple::new(iri("http://ex/a"), iri("http://other/name"), lit("A")),
        Triple::new(
            iri("http://ex/a"),
            iri("http://ex/child"),
            iri("http://ex/c"),
        ),
        Triple::new(iri("http://ex/c"), iri("http://other/name"), lit("C")),
        Triple::new(iri("http://ex/b"), iri("http://ex/name"), lit("B")),
    ];
    want.sort_by_key(|t| format!("{t:?}"));
    assert_eq!(got, want);
}

fn rdfa_with_prefixes(prefixes: usize, elements: usize) -> String {
    let decl: Vec<String> = (0..prefixes)
        .map(|i| format!("p{i}: http://ex/p{i}/"))
        .collect();
    let body: String = (0..elements)
        .map(|i| format!("<div about=\"http://ex/s{i}\" property=\"p0:v\">x</div>"))
        .collect();
    format!(
        "<html prefix=\"{}\"><body>{body}</body></html>",
        decl.join(" ")
    )
}

/// Review parser#7 (RDFa): every element copied the prefix map in scope
/// (2000 elements: 6 ms with no prefix declared, 184 ms under 1000). Shared,
/// 100 times the prefixes cost about the same (bound: 4).
#[test]
fn rdfa_elements_share_the_prefix_map() {
    let elements = 2000;
    let small = rdfa_with_prefixes(10, elements);
    let large = rdfa_with_prefixes(1000, elements);
    let ts = rdfa::parse_str(&large).unwrap();
    assert_eq!(
        ts.iter()
            .filter(|t| t.predicate == iri("http://ex/p0/v"))
            .count(),
        elements
    );
    let t_small = fastest(|| {
        rdfa::parse_str(&small).unwrap();
    });
    let t_large = fastest(|| {
        rdfa::parse_str(&large).unwrap();
    });
    let ratio = t_large.as_secs_f64() / t_small.as_secs_f64().max(1e-6);
    assert!(
        ratio < 4.0 || t_large < Duration::from_millis(30),
        "100x the prefixes took {ratio:.1}x as long ({t_small:?} -> {t_large:?})"
    );
}

/// RDFa Core §7.5 step 3: a prefix mapping applies to the element that
/// declares it and its descendants, not to its siblings.
#[test]
fn an_rdfa_prefix_is_scoped_to_its_element() {
    let doc = "<html prefix=\"a: http://ex/a/\"><body>\
               <div prefix=\"a: http://ex/inner/\" about=\"http://ex/s1\" property=\"a:p\">x</div>\
               <div about=\"http://ex/s2\" property=\"a:p\">y</div></body></html>";
    let ts = rdfa::parse_str(doc).unwrap();
    assert!(ts.contains(&Triple::new(
        iri("http://ex/s1"),
        iri("http://ex/inner/p"),
        lit("x")
    )));
    assert!(ts.contains(&Triple::new(
        iri("http://ex/s2"),
        iri("http://ex/a/p"),
        lit("y")
    )));
}

/// Review parser#7, by another route (found in review): a node object
/// with its own `@context`, even an empty one, still copied the whole
/// context in scope (5000 `{"@context": {}}` under 4000 terms: 1.8 s), and
/// each member of a `@context` array copied the context built so far
/// (8000 one-term members: 10.5 s for a 238 KB document, quadratic). A
/// node's definitions are now a layer over the shared ones, and an array's
/// members fill one layer: 16 times the terms in scope cost about the same
/// (bound: 4), and 4 times the array members about 4 times as much (bound:
/// 8, which a quadratic cost, at 16, misses).
#[test]
fn jsonld_node_contexts_and_context_arrays_add_layers_not_copies() {
    let nodes = 5000;
    let doc = |terms: usize, node: &str| {
        let defs: Vec<String> = (0..terms)
            .map(|i| format!("\"t{i}\": \"http://ex/t{i}\""))
            .collect();
        format!(
            "{{\"@context\": {{{}}}, \"@graph\": [{}]}}",
            defs.join(", "),
            vec![node; nodes].join(", ")
        )
    };
    for (node, triples) in [
        (r#"{"@context": {}}"#, 0),
        (
            r#"{"@context": {"x": "http://ex/x"}, "@id": "http://ex/s", "x": "v", "t0": "w"}"#,
            2 * nodes,
        ),
    ] {
        let small = doc(250, node);
        let large = doc(4000, node);
        assert_eq!(jsonld::parse_str(&large).unwrap().len(), triples, "{node}");
        let t_small = fastest(|| {
            jsonld::parse_str(&small).unwrap();
        });
        let t_large = fastest(|| {
            jsonld::parse_str(&large).unwrap();
        });
        let ratio = t_large.as_secs_f64() / t_small.as_secs_f64().max(1e-6);
        assert!(
            ratio < 4.0 || t_large < Duration::from_millis(30),
            "{node}: 16x the context terms took {ratio:.1}x as long ({t_small:?} -> {t_large:?})"
        );
    }
    let array = |members: usize| {
        let items: Vec<String> = (0..members)
            .map(|i| format!("{{\"t{i}\": \"http://ex/t{i}\"}}"))
            .collect();
        format!(
            "{{\"@context\": [{}], \"@id\": \"http://ex/s\", \"t0\": \"a\", \"t{}\": \"b\"}}",
            items.join(", "),
            members - 1
        )
    };
    let small = array(2000);
    let large = array(8000);
    assert_eq!(
        jsonld::parse_str(&large).unwrap(),
        [
            Triple::new(iri("http://ex/s"), iri("http://ex/t0"), lit("a")),
            Triple::new(iri("http://ex/s"), iri("http://ex/t7999"), lit("b")),
        ]
    );
    let t_small = fastest(|| {
        jsonld::parse_str(&small).unwrap();
    });
    let t_large = fastest(|| {
        jsonld::parse_str(&large).unwrap();
    });
    let ratio = t_large.as_secs_f64() / t_small.as_secs_f64().max(1e-6);
    assert!(
        ratio < 8.0 || t_large < Duration::from_millis(30),
        "4x the context array members took {ratio:.1}x as long ({t_small:?} -> {t_large:?})"
    );
}

/// The layers keep JSON-LD's scoping (JSON-LD 1.1 §4.1, context
/// processing): a node's `"term": null` removes the outer definition for
/// that node and what it contains (the term falls back to `@vocab`), not
/// for its siblings; a later member of a context array overrides an
/// earlier one; a `null` member resets the context.
#[test]
fn jsonld_context_layers_keep_definitions_scoped() {
    let doc = r#"{"@context": {"@vocab": "http://v/", "ex": "http://ex/", "name": "http://ex/name"},
        "@graph": [
          {"@id": "ex:a", "@context": {"name": null}, "name": "A",
           "ex:child": {"@id": "ex:c", "name": "C"}},
          {"@id": "ex:b", "name": "B"},
          {"@id": "ex:d", "@context": [{"name": "http://one/name"}, {"name": "http://two/name"}],
           "name": "D"},
          {"@id": "http://ex/e", "@context": [null, {"name": "http://three/name"}],
           "name": "E", "other": "dropped: no @vocab after the reset"}
        ]}"#;
    let mut got = jsonld::parse_str(doc).unwrap();
    got.sort_by_key(|t| format!("{t:?}"));
    let mut want = vec![
        Triple::new(iri("http://ex/a"), iri("http://v/name"), lit("A")),
        Triple::new(
            iri("http://ex/a"),
            iri("http://ex/child"),
            iri("http://ex/c"),
        ),
        Triple::new(iri("http://ex/c"), iri("http://v/name"), lit("C")),
        Triple::new(iri("http://ex/b"), iri("http://ex/name"), lit("B")),
        Triple::new(iri("http://ex/d"), iri("http://two/name"), lit("D")),
        Triple::new(iri("http://ex/e"), iri("http://three/name"), lit("E")),
    ];
    want.sort_by_key(|t| format!("{t:?}"));
    assert_eq!(got, want);
}

/// Review parser#7 (RDFa), by the same route: an element declaring a
/// prefix still copied the whole map in scope (2000 such elements: 6 ms
/// under 10 prefixes, 140 ms under 1000). Its mappings are now a layer over
/// the shared ones: 100 times the prefixes cost about the same (bound: 4).
#[test]
fn rdfa_elements_declaring_a_prefix_add_a_layer_not_a_copy() {
    let elements = 4000;
    let doc = |prefixes: usize| {
        let decl: Vec<String> = (0..prefixes)
            .map(|i| format!("p{i}: http://ex/p{i}/"))
            .collect();
        let body: String = (0..elements)
            .map(|i| {
                format!(
                    "<div prefix=\"q: http://ex/q/\" about=\"http://ex/s{i}\" property=\"q:v\">x</div>"
                )
            })
            .collect();
        format!(
            "<html prefix=\"{}\"><body>{body}</body></html>",
            decl.join(" ")
        )
    };
    let small = doc(10);
    let large = doc(1000);
    let ts = rdfa::parse_str(&large).unwrap();
    assert_eq!(
        ts.iter()
            .filter(|t| t.predicate == iri("http://ex/q/v"))
            .count(),
        elements
    );
    let t_small = fastest(|| {
        rdfa::parse_str(&small).unwrap();
    });
    let t_large = fastest(|| {
        rdfa::parse_str(&large).unwrap();
    });
    let ratio = t_large.as_secs_f64() / t_small.as_secs_f64().max(1e-6);
    assert!(
        ratio < 4.0 || t_large < Duration::from_millis(30),
        "100x the prefixes took {ratio:.1}x as long ({t_small:?} -> {t_large:?})"
    );
}

/// RDFa Core §7.5 step 3, nested: an inner mapping (`prefix` or `xmlns:`)
/// hides an outer one for the element and its descendants only.
#[test]
fn an_inner_rdfa_prefix_hides_an_outer_one_in_its_subtree() {
    let doc = "<html prefix=\"a: http://ex/a/\"><body>\
               <div prefix=\"a: http://ex/inner/\">\
                 <span about=\"http://ex/s1\" property=\"a:p\">x</span>\
                 <div xmlns:a=\"http://ex/x/\"><span about=\"http://ex/s3\" property=\"a:p\">z</span></div>\
                 <span about=\"http://ex/s4\" property=\"a:p\">w</span>\
               </div>\
               <div about=\"http://ex/s2\" property=\"a:p\">y</div></body></html>";
    let ts = rdfa::parse_str(doc).unwrap();
    for (s, p, o) in [
        ("http://ex/s1", "http://ex/inner/p", "x"),
        ("http://ex/s3", "http://ex/x/p", "z"),
        ("http://ex/s4", "http://ex/inner/p", "w"),
        ("http://ex/s2", "http://ex/a/p", "y"),
    ] {
        assert!(
            ts.contains(&Triple::new(iri(s), iri(p), lit(o))),
            "{s} {p} {o:?} missing from {ts:?}"
        );
    }
}

// ---- one token is bounded in the streaming readers (parser#5) -----------------

/// A reader of `head`, then `n` bytes of `fill`, then `tail`, holding none
/// of the `n` bytes in memory.
fn generated(head: &str, fill: u8, n: usize, tail: &str) -> impl std::io::BufRead {
    let r = Cursor::new(head.as_bytes().to_vec())
        .chain(std::io::repeat(fill).take(n as u64))
        .chain(Cursor::new(tail.as_bytes().to_vec()));
    std::io::BufReader::with_capacity(64 * 1024, r)
}

/// Import `reader` tolerantly with a token cap of `cap` bytes: the stored
/// statements' objects and the logged errors.
fn import_capped(
    reader: impl std::io::BufRead,
    format: gstore::parser::import::RdfFormat,
    cap: usize,
) -> (Vec<Term>, Vec<String>) {
    use gstore::parser::import::{import_reader, ImportOptions};
    use gstore::parser::nquads::Quad;
    use gstore::parser::stream::SyntaxError;
    // Small batches: the batch vector is allocated for `batch_size` quads
    // up front (about 19 MB at the default), which is not what is measured.
    let opts = ImportOptions {
        max_token_bytes: cap,
        batch_size: 16,
        ..ImportOptions::tolerant()
    };
    let mut errors: Vec<(String, SyntaxError)> = Vec::new();
    let mut objects = Vec::new();
    import_reader(
        reader,
        format,
        "doc",
        &opts,
        &mut errors,
        &mut |b: &mut Vec<Quad>| {
            objects.extend(b.drain(..).map(|q| q.object));
            Ok(())
        },
    )
    .unwrap();
    let errors = errors
        .into_iter()
        .map(|(_, e)| format!("line {}: {}", e.line, e.message))
        .collect();
    (objects, errors)
}

/// Review parser#5: a token of a streaming reader was held whole however
/// long — an unterminated `"""` string, a newline-free short string or
/// IRI. Now the statement fails once the token passes the cap
/// (`ImportOptions::max_token_bytes`, `[upload] rdf_max_token_size`), having
/// held about that much of it, and a tolerant import goes on with the next
/// statement.
#[test]
fn a_turtle_token_longer_than_the_cap_fails_its_statement_only() {
    use gstore::parser::import::RdfFormat;
    let cap = 256 * 1024;
    let n = 16 * MIB;
    let next = "\n<http://ex/s> <http://ex/p> <http://ex/next> .\n";
    for (what, head, fill, tail) in [
        ("short string", "<http://ex/s> <http://ex/p> \"", b'a', next),
        (
            "IRI",
            "<http://ex/s> <http://ex/p> <http://ex/",
            b'a',
            ">\n<http://ex/s> <http://ex/p> <http://ex/next> .\n",
        ),
        (
            "prefixed name",
            "@prefix e: <http://ex/> .\n<http://ex/s> <http://ex/p> e:",
            b'a',
            next,
        ),
        ("number", "<http://ex/s> <http://ex/p> ", b'7', next),
    ] {
        let ((objects, errors), peak) =
            peak_during(|| import_capped(generated(head, fill, n, tail), RdfFormat::Turtle, cap));
        assert_eq!(objects, [iri("http://ex/next")], "{what}: {errors:?}");
        assert_eq!(errors.len(), 1, "{what}: {errors:?}");
        assert!(
            errors[0].contains("rdf_max_token_size"),
            "{what}: {errors:?}"
        );
        eprintln!("measured: turtle {what} of {n} bytes, cap {cap}: peak {peak} bytes");
        assert!(
            peak < 4 * MIB,
            "{what}: held {peak} bytes for a {n}-byte token"
        );
    }
    // An unterminated long string runs to the end of the input: one error,
    // and only the cap's worth of it held.
    let ((objects, errors), peak) = peak_during(|| {
        import_capped(
            generated("<http://ex/s> <http://ex/p> \"\"\"", b'a', n, ""),
            RdfFormat::Turtle,
            cap,
        )
    });
    assert!(objects.is_empty());
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("rdf_max_token_size"), "{errors:?}");
    eprintln!("measured: turtle unterminated long string of {n} bytes, cap {cap}: peak {peak}");
    assert!(
        peak < 4 * MIB,
        "held {peak} bytes for an unterminated long string"
    );
    // TriG: the same in a graph block.
    let ((objects, errors), _) = peak_during(|| {
        import_capped(
            generated(
                "<http://ex/g> { <http://ex/s> <http://ex/p> \"",
                b'a',
                n,
                "\"\n<http://ex/s> <http://ex/p> <http://ex/next> }\n",
            ),
            RdfFormat::TriG,
            cap,
        )
    });
    assert_eq!(objects, [iri("http://ex/next")], "{errors:?}");
    assert_eq!(errors.len(), 1, "{errors:?}");
}

/// Review parser#5: an N-Triples / N-Quads line was read whole, and the
/// scanner copied it into a `Vec<char>` (six times the line in memory). A
/// line within the cap now takes about twice its size (the line and its
/// term); a longer one is an error of its statement.
#[test]
fn an_nquads_line_is_held_once_and_bounded_by_the_cap() {
    use gstore::parser::import::RdfFormat;
    let n = 16 * MIB;
    let tail = "\" <http://ex/g> .\n<http://ex/s> <http://ex/p> <http://ex/next> .\n";
    let ((objects, errors), peak) = peak_during(|| {
        import_capped(
            generated("<http://ex/s> <http://ex/p> \"", b'a', n, tail),
            RdfFormat::NQuads,
            usize::MAX,
        )
    });
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(objects.len(), 2);
    assert_eq!(objects[0], lit(&"a".repeat(n)));
    drop(objects);
    eprintln!("measured: N-Quads line of {n} bytes, no cap: peak {peak} bytes");
    assert!(peak < 4 * n, "held {peak} bytes for a {n}-byte line");
    for format in [RdfFormat::NQuads, RdfFormat::NTriples] {
        let ((objects, errors), peak) = peak_during(|| {
            import_capped(
                generated(
                    "<http://ex/s> <http://ex/p> \"",
                    b'a',
                    n,
                    "\" .\n<http://ex/s> <http://ex/p> <http://ex/next> .\n",
                ),
                format,
                MIB,
            )
        });
        assert_eq!(objects, [iri("http://ex/next")], "{format:?}");
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].starts_with("line 1:") && errors[0].contains("rdf_max_token_size"),
            "{errors:?}"
        );
        eprintln!("measured: {format:?} line of {n} bytes, cap 1 MiB: peak {peak} bytes");
        assert!(peak < 4 * MIB, "{format:?}: held {peak} bytes");
    }
}

// ---- the SPARQL tokenizer reads the request in place (parser#8) ---------------

/// Review parser#8: the tokenizer copied the request into a `Vec<char>`
/// plus a byte-offset table, 12 bytes per request byte, before parsing.
/// Now whitespace and comments cost nothing, and a long string literal
/// about its own size.
#[test]
fn the_sparql_tokenizer_allocates_per_token_not_per_byte() {
    use gstore::parser::sparql::lexer::tokenize;
    let n = 16 * MIB;
    let blank = format!("{}SELECT * WHERE {{ ?s ?p ?o }}", " \n".repeat(n / 2));
    let (tokens, peak) = peak_during(|| tokenize(&blank).unwrap());
    assert_eq!(tokens.len(), 9);
    eprintln!("measured: tokenize {n} bytes of whitespace: peak {peak} bytes");
    assert!(peak < MIB, "{peak} bytes for {n} bytes of whitespace");
    let comments = format!("{}ASK {{}}", "# a comment line\n".repeat(n / 17));
    let (tokens, peak) = peak_during(|| tokenize(&comments).unwrap());
    assert_eq!(tokens.len(), 4);
    eprintln!("measured: tokenize {n} bytes of comments: peak {peak} bytes");
    assert!(peak < MIB, "{peak} bytes for {n} bytes of comments");
    let literal = format!("ASK {{ FILTER(?x = \"{}\") }}", "é".repeat(n / 2));
    let (tokens, peak) = peak_during(|| tokenize(&literal).unwrap());
    assert_eq!(tokens.len(), 10);
    eprintln!("measured: tokenize a {n}-byte string literal: peak {peak} bytes");
    assert!(peak < 3 * n, "{peak} bytes for a {n}-byte literal");
}

// ---- Turtle numbers follow the grammar (parser#10) ----------------------------

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

fn typed(v: &str, local: &str) -> Term {
    Term::typed_literal(v, format!("{XSD}{local}"))
}

/// Import `doc` tolerantly (no token cap): the stored objects and the
/// logged errors (`line N: message`).
fn import_tolerant(
    doc: &[u8],
    format: gstore::parser::import::RdfFormat,
) -> (Vec<Term>, Vec<String>) {
    import_capped(Cursor::new(doc.to_vec()), format, usize::MAX)
}

/// Import `doc` strictly: `Err` with the message of the first failure.
fn import_strict(doc: &[u8], format: gstore::parser::import::RdfFormat) -> Result<(), String> {
    use gstore::parser::import::{import_reader, ImportOptions};
    use gstore::parser::nquads::Quad;
    use gstore::parser::stream::SyntaxError;
    let mut errors: Vec<(String, SyntaxError)> = Vec::new();
    import_reader(
        Cursor::new(doc.to_vec()),
        format,
        "doc",
        &ImportOptions::strict(),
        &mut errors,
        &mut |b: &mut Vec<Quad>| {
            b.clear();
            Ok(())
        },
    )
    .map(drop)
    .map_err(|e| e.to_string())
}

/// Turtle 1.1 §6.5, productions [19]-[21] and [154]:
///
/// ```text
/// INTEGER  ::= [+-]? [0-9]+
/// DECIMAL  ::= [+-]? [0-9]* '.' [0-9]+
/// DOUBLE   ::= [+-]? ([0-9]+ '.' [0-9]* EXPONENT | '.' [0-9]+ EXPONENT | [0-9]+ EXPONENT)
/// EXPONENT ::= [eE] [+-]? [0-9]+
/// ```
///
/// and §7.2: the literal's lexical form is the token as written. Review
/// parser#10: `1.e5` (a DOUBLE: digits, '.', no fraction, exponent) was
/// refused (in a tolerant import it became `"1"^^xsd:integer`).
#[test]
fn turtle_numbers_follow_the_integer_decimal_double_productions() {
    let cases = [
        ("123", "integer"),
        ("-123", "integer"),
        ("+123", "integer"),
        ("007", "integer"),
        ("123.0", "decimal"),
        (".1", "decimal"),
        ("-.1", "decimal"),
        ("+123.0", "decimal"),
        ("123.0e1", "double"),
        ("-123e-1", "double"),
        ("123.E+1", "double"),
        ("1.e5", "double"),
        (".5e-3", "double"),
        ("1E5", "double"),
        ("+1.5E+3", "double"),
    ];
    for (number, dt) in cases {
        let want = vec![Triple::new(
            iri("http://ex/s"),
            iri("http://ex/p"),
            typed(number, dt),
        )];
        for doc in [
            format!("<http://ex/s> <http://ex/p> {number} ."),
            format!("<http://ex/s> <http://ex/p> {number} , {number}."),
            format!("<http://ex/s> <http://ex/p> {number};."),
        ] {
            let mut got = turtle::parse_str(&doc).unwrap_or_else(|e| panic!("{doc}: {e}"));
            got.dedup();
            assert_eq!(got, want, "{doc}");
        }
    }
    // `123.` is the INTEGER 123 and the statement's '.' (neither a DECIMAL,
    // which needs a digit after '.', nor a DOUBLE, which needs an exponent).
    assert_eq!(
        turtle::parse_str("<http://ex/s> <http://ex/p> 123.").unwrap(),
        [Triple::new(
            iri("http://ex/s"),
            iri("http://ex/p"),
            typed("123", "integer")
        )]
    );
    // TriG uses the same terminals.
    let quads =
        gstore::parser::trig::parse_str("<http://ex/g> { <http://ex/s> <http://ex/p> 1.e5 }")
            .unwrap();
    assert_eq!(quads.len(), 1);
    assert_eq!(quads[0].object, typed("1.e5", "double"));
    assert_eq!(quads[0].graph, Some(iri("http://ex/g")));
}

/// Review parser#10: the reader took any run of digits, dots and `e`s as a
/// number, so `1e`, `1E+`, `1.5.5` and `1e5e5` were stored as numbers (the
/// statement counted as loaded, nothing logged). The terminals above match
/// a prefix of each, and what is left is a syntax error: the statement
/// fails, in strict and tolerant imports alike, and the next one loads.
#[test]
fn malformed_turtle_numbers_fail_their_statement() {
    use gstore::parser::import::RdfFormat;
    for bad in [
        "1e", "1E+", "1e-", "1.5e", "1.5.5", "1e5e5", "1e5.5", "123abc", "0x12", "+-1", "+", "-",
        ".", "..5",
    ] {
        let doc = format!("<http://ex/s> <http://ex/p> {bad} .\n");
        assert!(turtle::parse_str(&doc).is_err(), "{bad} was accepted");
        let doc = format!("{doc}<http://ex/s> <http://ex/p> <http://ex/next> .\n");
        let (objects, errors) = import_tolerant(doc.as_bytes(), RdfFormat::Turtle);
        assert_eq!(objects, [iri("http://ex/next")], "{bad}: {errors:?}");
        assert_eq!(errors.len(), 1, "{bad}: {errors:?}");
        assert!(errors[0].starts_with("line 1:"), "{bad}: {errors:?}");
    }
    // TriG: a statement inside a block, whose final '.' is optional.
    assert!(
        gstore::parser::trig::parse_str("<http://ex/g> { <http://ex/s> <http://ex/p> 1.5.5 }")
            .is_err()
    );
    assert!(
        gstore::parser::trig::parse_str("<http://ex/g> { <http://ex/s> <http://ex/p> 1e }")
            .is_err()
    );
}

/// The longest-match rule splits `1.5.5` into the DECIMALs `1.5` and `.5`:
/// in a collection, where a term may follow a term, that is two items
/// (Turtle 1.1 §6.5 and §7.3, collections).
#[test]
fn turtle_numbers_are_split_by_longest_match() {
    let ts = turtle::parse_str("<http://ex/s> <http://ex/p> ( 1.5.5 1e5.5 ) .").unwrap();
    let first = iri(&format!("{RDF}first"));
    let items: Vec<&Term> = ts
        .iter()
        .filter(|t| t.predicate == first)
        .map(|t| &t.object)
        .collect();
    assert_eq!(
        items,
        [
            &typed("1.5", "decimal"),
            &typed(".5", "decimal"),
            &typed("1e5", "double"),
            &typed(".5", "decimal")
        ]
    );
}

// ---- a prefix may hold inner dots (parser#11) ---------------------------------

/// Turtle 1.1 §6.5 [167]: `PN_PREFIX ::= PN_CHARS_BASE ((PN_CHARS | '.')*
/// PN_CHARS)?` — a prefix may hold dots, but not start or end with one.
/// Review parser#11: `@prefix ex.v2: <…>` was accepted, but `ex.v2:s` then
/// stopped at the dot ("expected a prefixed name or IRI"), so every
/// statement using the prefix failed.
#[test]
fn a_turtle_prefix_may_hold_inner_dots() {
    let doc = "@prefix ex.v2: <http://ex/> .\n\
               PREFIX a..b: <http://ab/>\n\
               ex.v2:s ex.v2:p ex.v2:o.\n\
               a..b:s a..b:p a..b:o , ex.v2:o ;\n    ex.v2:q true.\n";
    let ts = turtle::parse_str(doc).unwrap();
    assert_eq!(
        ts,
        [
            Triple::new(iri("http://ex/s"), iri("http://ex/p"), iri("http://ex/o")),
            Triple::new(iri("http://ab/s"), iri("http://ab/p"), iri("http://ab/o")),
            Triple::new(iri("http://ab/s"), iri("http://ab/p"), iri("http://ex/o")),
            Triple::new(
                iri("http://ab/s"),
                iri("http://ex/q"),
                typed("true", "boolean")
            ),
        ]
    );
    // TriG: as a graph label, after GRAPH and inside the block.
    let quads = gstore::parser::trig::parse_str(
        "@prefix g.x: <http://g/> .\ng.x:G { g.x:s g.x:p g.x:o }\nGRAPH g.x:H { g.x:s g.x:p g.x:o . }\n",
    )
    .unwrap();
    let graphs: Vec<_> = quads.iter().map(|q| q.graph.clone()).collect();
    assert_eq!(graphs, [Some(iri("http://g/G")), Some(iri("http://g/H"))]);
    assert!(quads
        .iter()
        .all(|q| q.subject == iri("http://g/s") && q.object == iri("http://g/o")));
    // Through the importer (the review: a tolerant import stored nothing and
    // logged one failure per statement using the prefix).
    let (objects, errors) = import_tolerant(
        b"@prefix ex.v2: <http://ex/> .\nex.v2:s ex.v2:p ex.v2:o .\n<http://ex/s> <http://ex/p> ex.v2:o2 .\n",
        gstore::parser::import::RdfFormat::Turtle,
    );
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(objects, [iri("http://ex/o"), iri("http://ex/o2")]);
}

/// A prefix that starts or ends with '.' is not a PN_PREFIX, and no name of
/// the document could use it: the directive is refused, not stored.
#[test]
fn a_turtle_prefix_may_not_start_or_end_with_a_dot() {
    for decl in [
        "@prefix ex.: <http://ex/> .",
        "@prefix .ex: <http://ex/> .",
        "PREFIX ex.: <http://ex/>",
    ] {
        let err = turtle::parse_str(decl).unwrap_err().to_string();
        assert!(err.contains("invalid prefix name"), "{decl}: {err}");
    }
}

// ---- invalid UTF-8 in a comment (parser#12) -----------------------------------

/// Review parser#12: the bad-UTF-8 flag of a comment was charged to the
/// statement after it — `<o1>` below was dropped with "line 3: invalid
/// UTF-8" (strict: the import failed at line 3). A comment is whitespace
/// (Turtle 1.1 §6.4): the error is reported at the comment's line, and the
/// statements around it load.
#[test]
fn invalid_utf8_in_a_turtle_comment_is_reported_at_its_line() {
    use gstore::parser::import::RdfFormat;
    let all = [
        iri("http://ex/o0"),
        iri("http://ex/o1"),
        iri("http://ex/o2"),
    ];
    for (what, doc, line) in [
        (
            "comment line",
            &b"<http://ex/s> <http://ex/p> <http://ex/o0> .\n# caf\xe9\n<http://ex/s> <http://ex/p> <http://ex/o1> .\n<http://ex/s> <http://ex/p> <http://ex/o2> .\n"[..],
            2,
        ),
        (
            "trailing comment",
            &b"<http://ex/s> <http://ex/p> <http://ex/o0> . # caf\xe9\n<http://ex/s> <http://ex/p> <http://ex/o1> .\n<http://ex/s> <http://ex/p> <http://ex/o2> .\n"[..],
            1,
        ),
        (
            "comment inside a statement",
            &b"<http://ex/s> <http://ex/p> <http://ex/o0> .\n<http://ex/s> # caf\xe9\n  <http://ex/p> <http://ex/o1> .\n<http://ex/s> <http://ex/p> <http://ex/o2> .\n"[..],
            2,
        ),
    ] {
        for format in [RdfFormat::Turtle, RdfFormat::TriG] {
            let (objects, errors) = import_tolerant(doc, format);
            assert_eq!(objects, all, "{what} ({format:?}): {errors:?}");
            assert_eq!(
                errors,
                [format!("line {line}: invalid UTF-8 in a comment")],
                "{what} ({format:?})"
            );
        }
        // Strict: a Turtle document must be UTF-8, so the import still
        // fails — at the comment's line.
        let err = import_strict(doc, RdfFormat::Turtle).unwrap_err();
        assert!(err.contains(&format!("line {line}")), "{what}: {err}");
    }
}

/// Invalid UTF-8 in a term still fails the statement holding it — and only
/// that one, also when the byte comes right after the previous statement's
/// '.' (the reader looks ahead there, to tell a '.' from a number).
#[test]
fn invalid_utf8_in_a_turtle_term_fails_its_own_statement() {
    use gstore::parser::import::RdfFormat;
    let (objects, errors) = import_tolerant(
        b"<http://ex/s> <http://ex/p> \"caf\xe9\" .\n<http://ex/s> <http://ex/p> <http://ex/o1> .\n",
        RdfFormat::Turtle,
    );
    assert_eq!(objects, [iri("http://ex/o1")]);
    assert_eq!(errors, ["line 1: invalid UTF-8"]);
    let (objects, errors) = import_tolerant(
        b"<http://ex/s> <http://ex/p> <http://ex/o0> .\xe9 <http://ex/s> <http://ex/p> <http://ex/o1> .\n<http://ex/s> <http://ex/p> <http://ex/o2> .\n",
        RdfFormat::Turtle,
    );
    assert_eq!(
        objects,
        [iri("http://ex/o0"), iri("http://ex/o2")],
        "{errors:?}"
    );
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].starts_with("line 1:"), "{errors:?}");
}

// ---- blank graph labels in large documents (parser#14) ------------------------

/// A reader of N-Quads: `n` statements with distinct blank subjects, then,
/// for each of `graphs` blank graph labels, one quad in the graph and one
/// statement describing the label (generated as it is read).
struct BlankGraphDoc {
    n: usize,
    graphs: usize,
    i: usize,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for BlankGraphDoc {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        use std::io::Write;
        while self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
            let i = self.i;
            if i < self.n {
                writeln!(self.buf, "_:b{i} <http://ex/p> <http://ex/o> .")?;
            } else if i < self.n + self.graphs {
                let j = i - self.n;
                writeln!(
                    self.buf,
                    "<http://ex/s> <http://ex/p> <http://ex/o> _:g{j} ."
                )?;
                writeln!(self.buf, "_:g{j} <http://ex/desc> \"x\" .")?;
            } else {
                return Ok(0);
            }
            self.i += 1;
        }
        let k = out.len().min(self.buf.len() - self.pos);
        out[..k].copy_from_slice(&self.buf[self.pos..self.pos + k]);
        self.pos += k;
        Ok(k)
    }
}

/// Review parser#14: whether a blank graph label was used before as a
/// subject or object was tracked in a fixed 2^23-bit Bloom filter with two
/// hash bits: 4.5% false positives after 1M labels, 51% after 5M. A false
/// positive keeps the graph apart from the statements describing its label
/// (they stay a blank node). The filter now grows with the labels and keeps
/// the rate under 1% (the 5M-label measurement is a unit test of
/// `src/parser/import.rs`).
#[test]
fn blank_graph_labels_join_their_descriptions_in_a_large_document() {
    use gstore::parser::import::{import_reader, ImportOptions, RdfFormat};
    use gstore::parser::nquads::Quad;
    let (n, graphs) = (1 << 20, 4000);
    let doc = BlankGraphDoc {
        n,
        graphs,
        i: 0,
        buf: Vec::new(),
        pos: 0,
    };
    let desc = iri("http://ex/desc");
    let (mut joined, mut apart) = (0, 0);
    let mut errors: Vec<(String, gstore::parser::stream::SyntaxError)> = Vec::new();
    import_reader(
        std::io::BufReader::new(doc),
        RdfFormat::NQuads,
        "doc",
        &ImportOptions::strict(),
        &mut errors,
        &mut |b: &mut Vec<Quad>| {
            for q in b.drain(..).filter(|q| q.predicate == desc) {
                match q.subject {
                    Term::Iri(_) => joined += 1,
                    _ => apart += 1,
                }
            }
            Ok(())
        },
    )
    .unwrap();
    eprintln!("measured: {n} blank labels, {graphs} blank graphs: {apart} kept apart");
    assert_eq!(joined + apart, graphs);
    assert!(
        apart * 100 < graphs,
        "{apart} of {graphs} graphs kept apart"
    );
}
