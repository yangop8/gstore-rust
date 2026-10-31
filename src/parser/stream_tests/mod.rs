//! Differential tests: the streaming readers against the pre-streaming ones.
//!
//! `legacy_turtle` / `legacy_trig` are frozen copies of the readers this
//! crate had before they were rewritten to stream (whole document in a
//! `Vec<char>`); the N-Triples / N-Quads line parsers are unchanged, so the old
//! whole-document readers are re-created from them (`content.lines()` +
//! `parse_line`). Randomly generated documents — escapes, multi-byte UTF-8,
//! long literals, comments, CRLF, nested `[]` / `()` — are parsed by both: the
//! streaming reader through a reader handing out random 1..7-byte chunks (so
//! chunk boundaries fall inside escapes and multi-byte characters) with a tiny
//! random `BufReader` window. Results must be equal up to blank-node renaming
//! (anonymous nodes were `g~<n>`, now `genid-<n>`) and, for TriG, up to the
//! order of graphs (the old reader emitted the default graph first).

mod legacy_trig;
mod legacy_turtle;

use std::collections::HashMap;
use std::io::{BufReader, Read};

use crate::error::Result;
use crate::model::Term;
use crate::parser::nquads::{self, Quad};
use crate::parser::stream::{BlankLabeler, CollectQuads};
use crate::parser::{ntriples, trig, turtle};

/// xorshift64* — deterministic, dependency-free.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
}

/// A reader handing out 1..=7 bytes per `read`.
struct Trickle {
    data: Vec<u8>,
    pos: usize,
    rng: Rng,
}

impl Read for Trickle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = (1 + self.rng.below(7))
            .min(buf.len())
            .min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn trickle(doc: &str, seed: u64) -> BufReader<Trickle> {
    let mut rng = Rng(seed | 1);
    let cap = 1 + rng.below(16);
    BufReader::with_capacity(
        cap,
        Trickle {
            data: doc.as_bytes().to_vec(),
            pos: 0,
            rng,
        },
    )
}

fn stream_turtle(doc: &str, seed: u64) -> Result<Vec<Quad>> {
    let mut sink = CollectQuads::default();
    turtle::parse_reader(trickle(doc, seed), BlankLabeler::keep(), &mut sink)?;
    Ok(sink.quads)
}

fn stream_trig(doc: &str, seed: u64) -> Result<Vec<Quad>> {
    let mut sink = CollectQuads::default();
    trig::parse_reader(trickle(doc, seed), BlankLabeler::keep(), &mut sink)?;
    Ok(sink.quads)
}

fn stream_nt(doc: &str, seed: u64) -> Result<Vec<Quad>> {
    let mut sink = CollectQuads::default();
    ntriples::parse_reader(trickle(doc, seed), &BlankLabeler::keep(), &mut sink)?;
    Ok(sink.quads)
}

fn stream_nq(doc: &str, seed: u64) -> Result<Vec<Quad>> {
    let mut sink = CollectQuads::default();
    nquads::parse_reader(trickle(doc, seed), &BlankLabeler::keep(), &mut sink)?;
    Ok(sink.quads)
}

fn old_turtle(doc: &str) -> Result<Vec<Quad>> {
    Ok(legacy_turtle::parse_str(doc)?
        .into_iter()
        .map(|t| Quad::new(t.subject, t.predicate, t.object, None))
        .collect())
}

fn old_nt(doc: &str) -> Result<Vec<Quad>> {
    let mut out = Vec::new();
    for (i, line) in doc.lines().enumerate() {
        if let Some(t) = ntriples::parse_line(line, i + 1)? {
            out.push(Quad::new(t.subject, t.predicate, t.object, None));
        }
    }
    Ok(out)
}

fn old_nq(doc: &str) -> Result<Vec<Quad>> {
    let mut out = Vec::new();
    for (i, line) in doc.lines().enumerate() {
        if let Some(q) = nquads::parse_line(line, i + 1)? {
            out.push(q);
        }
    }
    Ok(out)
}

type Row = (String, String, String, Option<String>);

/// Quads with blank-node labels renamed `_:c<k>` by first appearance.
fn canon(quads: &[Quad]) -> Vec<Row> {
    let mut names: HashMap<String, usize> = HashMap::new();
    let mut name = |t: &Term| match t {
        Term::Blank(l) => {
            let n = names.len();
            format!("_:c{}", names.entry(l.clone()).or_insert(n))
        }
        other => other.to_string(),
    };
    quads
        .iter()
        .map(|q| {
            let g = q.graph.as_ref().map(&mut name);
            (name(&q.subject), name(&q.predicate), name(&q.object), g)
        })
        .collect()
}

/// TriG: group the quads by graph (default first, then graphs in order of
/// first appearance), keeping document order inside a graph, then
/// [`canon`]. Graph labels are ranked by their raw form, so a blank label
/// keeps its identity across the two readers (both keep explicit labels).
fn canon_by_graph(quads: &[Quad]) -> Vec<Row> {
    let mut rank: HashMap<Option<Term>, usize> = HashMap::new();
    rank.insert(None, 0);
    for q in quads {
        let n = rank.len();
        rank.entry(q.graph.clone()).or_insert(n);
    }
    let mut sorted: Vec<&Quad> = quads.iter().collect();
    sorted.sort_by_key(|q| rank[&q.graph]);
    canon(&sorted.into_iter().cloned().collect::<Vec<_>>())
}

// ---- generators -----------------------------------------------------------

/// Whitespace (sometimes with a comment) between tokens.
fn sep(r: &mut Rng) -> String {
    let mut s = r
        .pick(&[" ", "  ", "\t", "\n", "\r\n", " \t ", "\n  ", "\r\n\t"])
        .to_string();
    if r.chance(10) {
        s.push_str(r.pick(&[" # note é 漢 😀\n", " #c\r\n", "\t# a {brace} .\n"]));
    }
    s
}

fn iri(r: &mut Rng) -> String {
    let n = r.below(5);
    match r.below(11) {
        0 => format!("<http://ex.org/x{n}>"),
        1 => format!("<http://ex.org/é/漢{n}>"),
        2 => format!("<urn:x:{n}>"),
        3 => format!("<http://ex.org/esc\\u00E9\\U0001F600{n}>"),
        4 => format!("ex:a{n}"),
        5 => format!(":b{n}"),
        6 => format!("p2:c{n}"),
        7 => format!("ex:a.b{n}"),
        8 => format!("ex:a_b-c{n}"),
        9 => format!("<rel{n}>"),
        _ => format!("<../up/{n}#f>"),
    }
}

fn blank(r: &mut Rng) -> String {
    let n = r.below(4);
    if r.chance(50) {
        format!("_:b{n}")
    } else {
        format!("_:x{n}y")
    }
}

/// A string literal: short (both quote styles) or long, with escapes,
/// multi-byte characters and sometimes thousands of characters.
fn literal(r: &mut Rng) -> String {
    const PIECES: &[&str] = &[
        "abc", "a b", "é", "漢字", "😀", "\\\"", "\\\\", "\\n", "\\t", "\\r", "\\u00E9",
        "\\U0001F600", "#notcomment", ".", "{", "}", ";", ",", "\\'", "<x>", "_:no",
    ];
    let mut body = String::new();
    let parts = if r.chance(5) { 400 } else { r.below(6) };
    for _ in 0..parts {
        body.push_str(r.pick(PIECES));
    }
    let lit = match r.below(4) {
        0 => format!("'{}'", body.replace('"', "\\\"")),
        1 => {
            // A long literal may also hold raw newlines and lone quotes.
            let mut long = body.clone();
            if r.chance(50) {
                long.push_str("\nline two\r\n\"quoted\" end");
            }
            format!("\"\"\"{long}\"\"\"")
        }
        _ => format!("\"{body}\""),
    };
    let suffix = match r.below(6) {
        0 => "@en".to_string(),
        1 => "@en-US".to_string(),
        2 => "^^xsd:integer".to_string(),
        3 => "^^<http://ex.org/dt>".to_string(),
        _ => String::new(),
    };
    format!("{lit}{suffix}")
}

fn object(r: &mut Rng, depth: usize) -> String {
    match r.below(if depth < 3 { 12 } else { 8 }) {
        0 | 1 => iri(r),
        2 => blank(r),
        3 | 4 => literal(r),
        5 => r
            .pick(&["42", "-7", "+3", "3.14", "-0.5", "1e10", "2.5E-3", ".5"])
            .to_string(),
        6 => r.pick(&["true", "false", "[]", "()"]).to_string(),
        7 => iri(r),
        8 | 9 => format!("[{}{}{}]", sep(r), pol(r, depth + 1, false), sep(r)),
        _ => {
            let n = 1 + r.below(3);
            let items: Vec<String> = (0..n).map(|_| object(r, depth + 1)).collect();
            format!("({}{}{})", sep(r), items.join(&sep(r)), sep(r))
        }
    }
}

/// A predicate-object list; `trailing` allows a final `;` (before a `.`).
fn pol(r: &mut Rng, depth: usize, trailing: bool) -> String {
    let preds = 1 + r.below(3);
    let mut parts = Vec::new();
    for _ in 0..preds {
        let verb = if r.chance(15) { "a".to_string() } else { iri(r) };
        let objs = 1 + r.below(3);
        let os: Vec<String> = (0..objs).map(|_| object(r, depth)).collect();
        parts.push(format!("{verb}{}{}", sep(r), os.join(&format!("{},{}", sep(r), sep(r)))));
    }
    let mut s = parts.join(&format!("{};{}", sep(r), sep(r)));
    if trailing && r.chance(15) {
        s.push_str(" ;");
    }
    s
}

/// A complete Turtle statement (with its `.`).
fn statement(r: &mut Rng) -> String {
    let dot = if r.chance(70) { " ." } else { "\t." };
    match r.below(10) {
        0 => format!("[{}{}{}]{dot}", sep(r), pol(r, 1, false), sep(r)),
        1 => format!(
            "[{}{}{}]{}{}{dot}",
            sep(r),
            pol(r, 1, false),
            sep(r),
            sep(r),
            pol(r, 0, true)
        ),
        2 => format!("( {} {} ){}{}{dot}", iri(r), literal(r), sep(r), pol(r, 0, true)),
        3 => format!("{}{}{}{dot}", blank(r), sep(r), pol(r, 0, true)),
        _ => format!("{}{}{}{dot}", iri(r), sep(r), pol(r, 0, true)),
    }
}

const PREAMBLE: &str = "@prefix ex: <http://ex.org/ns#> .\n\
                        @prefix : <http://d.example/> .\r\n\
                        PREFIX p2: <http://p2.example/a/>\n\
                        @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\
                        # header comment\n\
                        @base <http://base.example/dir/sub/> .\n";

fn turtle_doc(r: &mut Rng) -> String {
    let mut doc = String::from(PREAMBLE);
    let n = 1 + r.below(12);
    for _ in 0..n {
        doc.push_str(&statement(r));
        doc.push_str(&sep(r));
    }
    doc
}

fn trig_doc(r: &mut Rng) -> String {
    let mut doc = String::from(PREAMBLE);
    let n = 1 + r.below(8);
    for _ in 0..n {
        match r.below(5) {
            0 => {
                doc.push_str(&statement(r));
            }
            k => {
                let label = match k {
                    1 => String::new(),
                    2 => format!(":g{} ", r.below(3)),
                    3 => format!("GRAPH <http://g.example/{}> ", r.below(3)),
                    _ => format!("_:bg{} ", r.below(2)),
                };
                let m = r.below(4);
                let stmts: Vec<String> = (0..m)
                    .map(|_| {
                        let s = statement(r);
                        // Inside a block the statements are `.`-separated.
                        s.trim_end_matches('.').trim_end().to_string()
                    })
                    .collect();
                let mut body = stmts.join(" .\n");
                if !stmts.is_empty() && r.chance(50) {
                    body.push_str(" .");
                }
                // No comment right before the `}`: the old reader decided
                // whether a block's last statement had its `.` by looking at
                // the body's last character, so a comment ending in `.` there
                // broke it (a bug of the old reader, fixed by streaming).
                let close = r.pick(&[" ", "\n", "\r\n", "\t"]);
                doc.push_str(&format!("{label}{{{}{body}{close}}}", sep(r)));
            }
        }
        doc.push_str(&sep(r));
    }
    doc
}

fn nt_term(r: &mut Rng, literal_ok: bool) -> String {
    let n = r.below(4);
    match r.below(if literal_ok { 7 } else { 3 }) {
        0 => format!("<http://ex.org/s{n}>"),
        1 => format!("<http://ex.org/é{n}\\u00E9>"),
        2 => format!("_:n{n}"),
        3 => "\"plain é 漢 😀 \\\"q\\\" \\\\ \\n \\t \\u00E9 \\U0001F600\"".to_string(),
        4 => format!("\"{}\"@en-GB", "x".repeat(if r.chance(5) { 3000 } else { 3 })),
        5 => "\"5\"^^<http://www.w3.org/2001/XMLSchema#integer>".to_string(),
        _ => "\"tab\tand space\"".to_string(),
    }
}

/// An N-Triples (or, with `quads`, N-Quads) document.
fn line_doc(r: &mut Rng, quads: bool) -> String {
    let mut doc = String::new();
    let n = 1 + r.below(15);
    for _ in 0..n {
        let s = nt_term(r, false);
        let p = format!("<http://ex.org/p{}>", r.below(3));
        let o = nt_term(r, true);
        let g = if quads && r.chance(60) {
            if r.chance(70) {
                format!(" <http://g.example/{}>", r.below(3))
            } else {
                format!(" _:g{}", r.below(2))
            }
        } else {
            String::new()
        };
        let ws = if r.chance(30) { "\t" } else { " " };
        let end = r.pick(&[" .", ".", " . # c é", " .", ""]);
        doc.push_str(&format!("{s}{ws}{p}{ws}{o}{g}{end}"));
        doc.push_str(r.pick(&["\n", "\r\n", "\n\n", "\n# comment 漢\n", "\n   \n"]));
    }
    doc
}

// ---- the differential tests -------------------------------------------------

const DOCS: u64 = 250;

#[test]
fn streaming_turtle_equals_the_old_reader_on_random_documents() {
    for seed in 1..=DOCS {
        let mut r = Rng(seed * 7919);
        let doc = turtle_doc(&mut r);
        let old = old_turtle(&doc).unwrap_or_else(|e| panic!("old reader: {e}\n{doc}"));
        for chunking in 0..3 {
            let new = stream_turtle(&doc, seed * 31 + chunking)
                .unwrap_or_else(|e| panic!("streaming reader: {e}\n{doc}"));
            assert_eq!(canon(&new), canon(&old), "seed {seed}\n{doc}");
        }
        // The whole-document API agrees too.
        let whole = turtle::parse_str(&doc).unwrap();
        assert_eq!(whole.len(), old.len());
    }
}

#[test]
fn streaming_trig_equals_the_old_reader_on_random_documents() {
    for seed in 1..=DOCS {
        let mut r = Rng(seed * 104_729);
        let doc = trig_doc(&mut r);
        let old = legacy_trig::parse_str(&doc).unwrap_or_else(|e| panic!("old reader: {e}\n{doc}"));
        for chunking in 0..3 {
            let new = stream_trig(&doc, seed * 17 + chunking)
                .unwrap_or_else(|e| panic!("streaming reader: {e}\n{doc}"));
            assert_eq!(canon_by_graph(&new), canon_by_graph(&old), "seed {seed}\n{doc}");
        }
    }
}

#[test]
fn streaming_ntriples_and_nquads_equal_the_old_readers() {
    for seed in 1..=DOCS {
        let mut r = Rng(seed * 6271);
        let nt = line_doc(&mut r, false);
        let nq = line_doc(&mut r, true);
        let (old_t, old_q) = (old_nt(&nt).unwrap(), old_nq(&nq).unwrap());
        for chunking in 0..3 {
            // N-Triples / N-Quads keep blank labels verbatim: compare exactly.
            assert_eq!(stream_nt(&nt, seed + chunking).unwrap(), old_t, "{nt}");
            assert_eq!(stream_nq(&nq, seed + chunking).unwrap(), old_q, "{nq}");
        }
        assert_eq!(nquads::parse_str(&nq).unwrap(), old_q);
        let triples: Vec<Quad> = ntriples::parse_str(&nt)
            .unwrap()
            .into_iter()
            .map(|t| Quad::new(t.subject, t.predicate, t.object, None))
            .collect();
        assert_eq!(triples, old_t);
    }
}

#[test]
fn both_readers_reject_the_same_broken_documents() {
    for seed in 1..=DOCS / 2 {
        let mut r = Rng(seed * 3571);
        // A bogus directive spliced in at a statement boundary.
        let mut doc = turtle_doc(&mut r);
        doc.push_str("\n@@bad .\n");
        doc.push_str(&statement(&mut r));
        assert!(old_turtle(&doc).is_err());
        assert!(stream_turtle(&doc, seed).is_err(), "{doc}");
        let mut tdoc = trig_doc(&mut r);
        tdoc.push_str("\n:g { <a> <b> }}\n");
        assert!(legacy_trig::parse_str(&tdoc).is_err());
        assert!(stream_trig(&tdoc, seed).is_err(), "{tdoc}");
        // A line missing its object.
        let mut nt = line_doc(&mut r, false);
        nt.push_str("<http://ex.org/a> <http://ex.org/b>\n");
        assert!(old_nt(&nt).is_err());
        assert!(stream_nt(&nt, seed).is_err());
    }
}

#[test]
fn existing_parser_corpus_agrees_with_the_old_reader() {
    // The documents of the reader unit tests (this module's siblings) that
    // the old reader accepted, plus the bundled datasets.
    let docs = [
        "@prefix : <http://e/> . :s :p :a , :b ; :q :c .",
        "<root>\t<name>\t\"Bookug Lobert\"\t.\n<root>\t<contain>\t<node0>\t.",
        "@prefix xsd: <http://www.w3.org/2001/XMLSchema#> . <a> <salary> \"2500\"^^xsd:integer . <a> <age> 42 . <a> <h> 1.5 .",
        "@base <http://ex/> . <a> <p> <b> .",
        "PREFIX : <http://e/>\n:s :p :o .",
        "@prefix : <http://e/> . :s :p [ :q :r ; :u :v ] .",
        "@prefix : <http://e/> . [ :p :o ; :q :r ] .",
        "@prefix : <http://e/> . :s :p ( :a :b :c ) .",
        "@prefix : <http://e/> . :s :p [ :q ( :a :b ) ] .",
        "@prefix : <http://e/> . _:g :p [ :q :r ] .",
        include_str!("../../../testdata/small/small.nt"),
    ];
    for doc in docs {
        let old = old_turtle(doc).unwrap();
        for chunking in 0..4 {
            assert_eq!(canon(&stream_turtle(doc, chunking).unwrap()), canon(&old), "{doc}");
        }
    }
}

#[test]
fn a_comment_ending_in_a_dot_before_a_block_end_is_a_comment() {
    // The old reader appended a `.` to a block body unless its text ended
    // with one — a trailing comment's `.` fooled it.
    let doc = "@prefix : <http://e/> .\n:g { :s :p :o # note .\n}\n";
    assert!(legacy_trig::parse_str(doc).is_err());
    let qs = stream_trig(doc, 3).unwrap();
    assert_eq!(qs.len(), 1);
    assert_eq!(qs[0].graph, Some(Term::iri("http://e/g")));
}

#[test]
fn bundled_lubm_dataset_streams_to_the_same_triples() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/lubm/lubm.nt");
    let doc = std::fs::read_to_string(path).unwrap();
    let old = old_turtle(&doc).unwrap();
    let mut sink = CollectQuads::default();
    turtle::parse_reader(
        BufReader::with_capacity(4096, std::fs::File::open(path).unwrap()),
        BlankLabeler::keep(),
        &mut sink,
    )
    .unwrap();
    assert!(old.len() > 10_000);
    assert_eq!(sink.quads, old);
}
