//! Every term a reader stores reads back, and every output escapes it
//! (review H-13: parser#1, sparqlconf#12; M9: sparqlconf#11; query#15).
//!
//! * One term validator (`gstore::model::term`) for every reader, the
//!   SPARQL lexer and UPDATE: a language tag is LANGTAG (`zh_CN` is read as
//!   `zh-CN` where the format writes tags as attribute or string values, an
//!   empty one means no tag there and is refused in SPARQL), a blank-node
//!   label is BLANK_NODE_LABEL (others get a stable valid label).
//! * The property: whatever any reader produces from generated documents,
//!   `ntriples::parse_term(export_key(dict_key(t)))` is `t` again, as is
//!   `Term::from_dict_key(dict_key(t))`.
//! * The redo log and incremental backups name stored terms exactly, so a
//!   database an older version filled with unchecked terms still replays
//!   and restores.
//! * UPDATE refuses the literals the tolerant loader refuses, so
//!   `gexport` -> `gbuild` loses nothing; `gbuild` exits 3 when it skipped
//!   statements.
//! * `IRI()` / `STRLANG()` check what they build, and N-Triples output
//!   (CONSTRUCT over HTTP, `gquery`) escapes every term: no stored term can
//!   forge a statement.
//! * Result serializers decode cells as the dictionary keys they are.
//!
//! Temp directories live under `CARGO_TARGET_TMPDIR` and are removed when a
//! test ends, also when it fails; servers run in-process on `127.0.0.1:0`.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use gstore::model::term::export_key;
use gstore::parser::{jsonld, nquads, ntriples, rdfa, rdfxml, trig, turtle};
use gstore::server::Server;
use gstore::sparql_results::{self, ResultFormat};
use gstore::{Database, QueryResult, Term, Triple};

// ---- plumbing ---------------------------------------------------------------

/// A temp directory removed on drop (also while a failing test unwinds).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let p = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("term_roundtrip_{tag}_{}_{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }

    fn join(&self, p: &str) -> PathBuf {
        self.0.join(p)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn lit(value: &str, lang: Option<&str>, datatype: Option<&str>) -> Term {
    Term::Literal {
        value: value.into(),
        datatype: datatype.map(Into::into),
        lang: lang.map(Into::into),
    }
}

/// Every statement of `db`'s default graph, as its dictionary keys.
fn all_keys(db: &mut Database) -> BTreeSet<Vec<String>> {
    match db.query("SELECT ?s ?p ?o WHERE { ?s ?p ?o }").unwrap() {
        QueryResult::Select(rs) => rs
            .rows
            .into_iter()
            .map(|r| r.into_iter().map(|c| c.unwrap()).collect())
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn select_cells(db: &mut Database, q: &str) -> Vec<Vec<Option<String>>> {
    match db.query(q).unwrap() {
        QueryResult::Select(rs) => rs.rows,
        other => panic!("{other:?}"),
    }
}

/// The language tag of the single literal object `triples` hold.
fn lang_of(triples: &[Triple]) -> Option<String> {
    let objects: Vec<&Term> = triples
        .iter()
        .map(|t| &t.object)
        .filter(|o| o.is_literal())
        .collect();
    assert_eq!(objects.len(), 1, "{triples:?}");
    match objects[0] {
        Term::Literal { lang, .. } => lang.clone(),
        _ => unreachable!(),
    }
}

/// The main property: `t` survives dict_key -> export_key -> the strict
/// N-Triples reader, and the dictionary-key decoder.
fn assert_round_trips(t: &Term, origin: &str) {
    let key = t.dict_key();
    assert_eq!(
        Term::from_dict_key(&key).as_ref(),
        Some(t),
        "{origin}: dictionary key {key:?} does not decode to {t:?}"
    );
    let exported = export_key(&key);
    match ntriples::parse_term(&exported) {
        Ok(back) => assert_eq!(&back, t, "{origin}: {exported:?} reads back as {back:?}"),
        Err(e) => panic!("{origin}: the export of {t:?} ({exported:?}) does not read back: {e}"),
    }
}

// ---- parser#1: one language-tag rule for every reader ------------------------

const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

fn rdfxml_doc(lang: &str) -> String {
    format!(
        "<rdf:RDF xmlns:rdf=\"{RDF}\" xmlns:ex=\"http://ex/\">\
         <rdf:Description rdf:about=\"http://ex/s\">\
         <ex:label xml:lang=\"{lang}\">x</ex:label></rdf:Description></rdf:RDF>"
    )
}

fn jsonld_doc(lang: &str) -> String {
    format!(
        "{{\"@id\": \"http://ex/s\", \"http://ex/label\": {{\"@value\": \"x\", \
         \"@language\": \"{lang}\"}}}}"
    )
}

fn jsonld_context_doc(lang: &str) -> String {
    format!(
        "{{\"@context\": {{\"@language\": \"{lang}\"}}, \"@id\": \"http://ex/s\", \
         \"http://ex/label\": \"x\"}}"
    )
}

fn rdfa_doc(lang: &str) -> String {
    format!(
        "<html><body><div about=\"http://ex/s\" lang=\"{lang}\">\
         <span property=\"http://ex/label\">x</span></div></body></html>"
    )
}

#[test]
fn every_reader_normalises_or_refuses_language_tags_alike() {
    type Reader = fn(&str) -> gstore::Result<Vec<Triple>>;
    type Doc = fn(&str) -> String;
    let readers: [(&str, Doc, Reader); 4] = [
        ("rdfxml", rdfxml_doc, rdfxml::parse_str),
        ("jsonld", jsonld_doc, jsonld::parse_str),
        ("jsonld @context", jsonld_context_doc, jsonld::parse_str),
        ("rdfa", rdfa_doc, rdfa::parse_str),
    ];
    for (name, doc, read) in readers {
        // `_` reads as `-` (POSIX locale names), whitespace around is dropped.
        for (raw, want) in [("zh_CN", "zh-CN"), ("en-GB", "en-GB"), (" fr ", "fr")] {
            let ts = read(&doc(raw)).unwrap_or_else(|e| panic!("{name} {raw:?}: {e}"));
            assert_eq!(lang_of(&ts).as_deref(), Some(want), "{name} {raw:?}");
        }
        // An empty tag means no tag in these formats.
        let ts = read(&doc("")).unwrap_or_else(|e| panic!("{name} \"\": {e}"));
        assert_eq!(lang_of(&ts), None, "{name}: an empty tag is no tag");
        // Anything else that is no LANGTAG is refused.
        for bad in ["en US", "en>bad", "-x", "en-", "1en", "en--GB"] {
            assert!(
                read(&doc(bad)).is_err(),
                "{name}: the tag {bad:?} is no language tag"
            );
        }
    }
    // The line and Turtle readers and the SPARQL lexer write LANGTAG
    // itself: only a valid tag is a literal there.
    for bad in ["@-", "@-x", "@1en", "@en-"] {
        let line = format!("<http://ex/s> <http://ex/p> \"x\"{bad} .");
        assert!(ntriples::parse_str(&line).is_err(), "N-Triples {bad}");
        assert!(nquads::parse_str(&line).is_err(), "N-Quads {bad}");
        assert!(turtle::parse_str(&line).is_err(), "Turtle {bad}");
    }
    let mut db = Database::new("lang");
    for bad in ["\"w\"@", "\"w\"@-", "\"w\"@-x", "\"w\"@en-"] {
        let q = format!("INSERT DATA {{ <http://ex/s> <http://ex/c> {bad} }}");
        assert!(db.query(&q).is_err(), "SPARQL {bad}");
    }
    assert!(all_keys(&mut db).is_empty());
}

#[test]
fn blank_labels_from_attribute_formats_are_valid_and_stable() {
    // JSON-LD: a label holding a space, used twice, is one node.
    let doc = "[{\"@id\": \"_:a b\", \"http://ex/p\": \"v\"}, \
               {\"@id\": \"_:a b\", \"http://ex/q\": \"w\"}, \
               {\"@id\": \"_:a_b\", \"http://ex/r\": \"z\"}]";
    let ts = jsonld::parse_str(doc).unwrap();
    let label = |t: &Triple| match &t.subject {
        Term::Blank(l) => l.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(ts.len(), 3);
    assert_eq!(label(&ts[0]), label(&ts[1]), "one label, one node");
    assert_ne!(label(&ts[0]), label(&ts[2]), "`a b` and `a_b` stay apart");
    for t in &ts {
        assert_round_trips(&t.subject, "jsonld label");
    }
    // RDF/XML: an `rdf:nodeID` ending in `.` (a valid NCName).
    let doc = format!(
        "<rdf:RDF xmlns:rdf=\"{RDF}\" xmlns:ex=\"http://ex/\">\
         <rdf:Description rdf:nodeID=\"g.\"><ex:p>v</ex:p></rdf:Description>\
         <rdf:Description rdf:nodeID=\"g\"><ex:p>w</ex:p></rdf:Description></rdf:RDF>"
    );
    let ts = rdfxml::parse_str(&doc).unwrap();
    assert_eq!(ts.len(), 2);
    assert_ne!(label(&ts[0]), label(&ts[1]), "`g.` and `g` are two nodes");
    for t in &ts {
        assert_round_trips(&t.subject, "rdfxml nodeID");
    }
}

// ---- the property, over generated documents ---------------------------------

/// A small deterministic generator (xorshift64*), so a failure reproduces.
struct Gen(u64);

impl Gen {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[(self.next() % xs.len() as u64) as usize]
    }

    /// A short string over `alphabet`.
    fn string(&mut self, alphabet: &[char], max: usize) -> String {
        let n = (self.next() % (max as u64 + 1)) as usize;
        (0..n).map(|_| *self.pick(alphabet)).collect()
    }
}

/// Characters that break terms: separators, brackets, quotes, escapes,
/// controls, dots, non-ASCII.
const NASTY: &[char] = &[
    'a', 'B', 'z', '0', '7', '_', '-', '.', ' ', '>', '<', '"', '\\', '\n', '\t', '\r', '\u{1}',
    '\u{7f}', 'é', '中', '#', ':', '@', '{', '}', '^', '|', '`', '&', '\'', '/', '%', '·',
];
/// Characters language tags are made of, plus a few that are not.
const TAGGY: &[char] = &['e', 'n', 'Z', 'h', '1', '9', '-', '_', ' ', '>', '@', '.'];

fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `s` as the body of an N-Triples / Turtle / SPARQL string literal.
fn nt_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c < ' ' => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[test]
fn every_term_any_reader_produces_round_trips() {
    let mut g = Gen(0x9e37_79b9_7f4a_7c15);
    let mut produced = 0usize;
    let mut db = Database::new("prop");
    let mut check = |ts: &[Triple], origin: &str| {
        for t in ts {
            for term in [&t.subject, &t.predicate, &t.object] {
                assert_round_trips(term, origin);
                produced += 1;
            }
        }
    };
    for _ in 0..400 {
        let label = g.string(NASTY, 6);
        let tag = g.string(TAGGY, 6);
        let value = g.string(NASTY, 8);
        let iri = format!("http://ex/{}", g.string(NASTY, 6));
        let dt = format!("http://ex/dt{}", g.string(NASTY, 4));
        // Line and Turtle-family readers (the label, tag and IRI raw).
        let line = format!(
            "_:{label} <{iri}> \"{}\"@{tag} .\n<http://ex/s> <http://ex/p> \"{}\"^^<{dt}> .\n",
            nt_escape(&value),
            nt_escape(&value)
        );
        if let Ok(ts) = ntriples::parse_str(&line) {
            check(&ts, "ntriples");
        }
        if let Ok(qs) = nquads::parse_str(&format!(
            "<http://ex/s> <http://ex/p> _:{label} _:{label} .\n{line}"
        )) {
            check(
                &qs.iter().map(|q| q.to_triple()).collect::<Vec<_>>(),
                "nquads",
            );
        }
        if let Ok(ts) = turtle::parse_str(&line) {
            check(&ts, "turtle");
        }
        if let Ok(qs) = trig::parse_str(&format!("_:{label} {{ {line} }}")) {
            check(
                &qs.iter().map(|q| q.to_triple()).collect::<Vec<_>>(),
                "trig",
            );
        }
        // Attribute / string formats (the values escaped for the format).
        let doc = format!(
            "<rdf:RDF xmlns:rdf=\"{RDF}\" xmlns:ex=\"http://ex/\">\
             <rdf:Description rdf:nodeID=\"{}\"><ex:p xml:lang=\"{}\">{}</ex:p>\
             <ex:q rdf:resource=\"{}\"/><ex:r rdf:datatype=\"{}\">{}</ex:r>\
             </rdf:Description></rdf:RDF>",
            xml_escape(&label),
            xml_escape(&tag),
            xml_escape(&value),
            xml_escape(&iri),
            xml_escape(&dt),
            xml_escape(&value)
        );
        if let Ok(ts) = rdfxml::parse_str(&doc) {
            check(&ts, "rdfxml");
        }
        let doc = format!(
            "{{\"@id\": {}, \"http://ex/p\": {{\"@value\": {}, \"@language\": {}}}, \
             \"http://ex/q\": {{\"@id\": {}}}, \"http://ex/r\": {{\"@value\": {}, \"@type\": {}}}}}",
            json_str(&format!("_:{label}")),
            json_str(&value),
            json_str(&tag),
            json_str(&iri),
            json_str(&value),
            json_str(&dt)
        );
        if let Ok(ts) = jsonld::parse_str(&doc) {
            check(&ts, "jsonld");
        }
        let doc = format!(
            "<html><body><div about=\"_:{}\" lang=\"{}\">\
             <span property=\"http://ex/p\">{}</span>\
             <span property=\"http://ex/q\" content=\"{}\" datatype=\"{}\"></span>\
             <div typeof=\"http://ex/T\"><span property=\"http://ex/r\">v</span></div>\
             </div></body></html>",
            xml_escape(&label),
            xml_escape(&tag),
            xml_escape(&value),
            xml_escape(&value),
            xml_escape(&dt)
        );
        if let Ok(ts) = rdfa::parse_str(&doc) {
            check(&ts, "rdfa");
        }
        // SPARQL UPDATE: what the store holds afterwards.
        let q = format!(
            "INSERT DATA {{ _:{label} <{iri}> \"{}\"@{tag} . <http://ex/s> <http://ex/p> \"{}\"^^<{dt}> }}",
            nt_escape(&value),
            nt_escape(&value)
        );
        let _ = db.query(&q);
    }
    for row in all_keys(&mut db) {
        for key in row {
            let t = Term::from_dict_key(&key).expect("a stored key decodes");
            assert_round_trips(&t, "sparql update");
            produced += 1;
        }
    }
    assert!(produced > 1000, "only {produced} terms were produced");
}

// ---- parser#1: redo replay and incremental restore ---------------------------

#[test]
fn redo_replay_and_incremental_restore_name_every_stored_term() {
    let tmp = TmpDir::new("redo");
    let mut db = Database::new("redo");
    db.enable_redo_log_in(tmp.join("logs")).unwrap();
    db.backup(tmp.join("base")).unwrap();
    // A database an older version filled: its readers stored these terms
    // unchecked (xml:lang="zh_CN", JSON-LD "@language": "" or "en US", a
    // JSON-LD label "a b", an RDF/XML nodeID "g.", a lenient reader's IRI).
    let s = || Term::iri("http://ex/s");
    let p = |n: &str| Term::iri(format!("http://ex/{n}"));
    let stored = vec![
        Triple::new(s(), p("a"), lit("x", Some("zh_CN"), None)),
        Triple::new(s(), p("b"), lit("w", Some(""), None)),
        Triple::new(s(), p("c"), lit("u", Some("en US"), None)),
        Triple::new(Term::blank("a b"), p("d"), lit("v", None, None)),
        Triple::new(Term::blank("x <http://ex/q> y"), p("d"), Term::blank("g.")),
        Triple::new(
            Term::iri("http://ex/a>b c\\u0041"),
            p("e"),
            Term::blank("g"),
        ),
        Triple::new(s(), p("f"), lit("1", None, Some("http://ex/dt>odd"))),
        Triple::new(s(), p("g"), lit("a\u{1}b\n\"q\"\\", None, None)),
        Triple::new(s(), p("h"), lit("x", Some("en> <http://ex/evil> ."), None)),
        Triple::new(s(), p("gone"), lit("x", Some("zh_TW"), None)),
    ];
    for t in &stored {
        assert!(db.insert_triple(t), "{t:?}");
    }
    assert!(db.remove_triple(stored.last().unwrap()));
    db.backup_incremental(tmp.join("inc1")).unwrap();
    let want = all_keys(&mut db);
    assert_eq!(want.len(), stored.len() - 1);

    let mut replayed = Database::new("replayed");
    let n = replayed
        .replay_redo_log(tmp.join("logs").join("redo.log"))
        .unwrap();
    assert_eq!(n, stored.len() + 1, "every record applies");
    assert_eq!(all_keys(&mut replayed), want, "replay");

    let mut restored =
        Database::restore_incremental(tmp.join("base"), &[tmp.join("inc1")]).unwrap();
    assert_eq!(all_keys(&mut restored), want, "incremental restore");
}

#[test]
fn redo_records_of_reader_terms_replay_after_a_load() {
    // The review's repro: RDF/XML zh_CN, JSON-LD empty language, SPARQL
    // terms, then replay — now stored normalised, and replayed exactly.
    let tmp = TmpDir::new("redo_load");
    std::fs::write(tmp.join("a.rdf"), rdfxml_doc("zh_CN")).unwrap();
    std::fs::write(
        tmp.join("b.jsonld"),
        "{\"@id\": \"_:a b\", \"http://ex/p\": {\"@value\": \"v\", \"@language\": \"\"}}",
    )
    .unwrap();
    let mut db = Database::new("redo_load");
    db.enable_redo_log_in(tmp.join("logs")).unwrap();
    for f in ["a.rdf", "b.jsonld"] {
        let path = tmp.join(f);
        db.query(&format!("LOAD <file://{}>", path.display()))
            .unwrap_or_else(|e| panic!("LOAD {f}: {e}"));
    }
    db.query("INSERT DATA { <http://ex/s> <http://ex/c> \"w\"@en-GB }")
        .unwrap();
    let want = all_keys(&mut db);
    assert_eq!(want.len(), 3);
    assert!(
        want.iter().any(|r| r[2] == "\"x\"@zh-CN"),
        "zh_CN is stored as zh-CN: {want:?}"
    );
    let mut replayed = Database::new("replayed");
    replayed
        .replay_redo_log(tmp.join("logs").join("redo.log"))
        .unwrap();
    assert_eq!(all_keys(&mut replayed), want);
}

// ---- sparqlconf#12: UPDATE and the loader agree; gexport -> gbuild -----------

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

#[test]
fn update_refuses_the_literals_the_loader_refuses() {
    let mut db = Database::new("lits");
    for bad in [
        format!("\"x\"^^<{XSD}integer>"),
        format!("\"300\"^^<{XSD}byte>"),
        format!("\"9223372036854775808\"^^<{XSD}integer>"),
        format!("\"NaN\"^^<{XSD}double>"),
    ] {
        let q = format!("INSERT DATA {{ <http://ex/s> <http://ex/p> {bad} }}");
        let err = db.query(&q).expect_err(&q).to_string();
        assert!(err.contains("loader refuses"), "{q}: {err}");
    }
    // Built by the engine: arbitrary-precision arithmetic, STRDT.
    for q in [
        "INSERT { <http://ex/x> <http://ex/big> ?v } WHERE { BIND(9223372036854775807 + 1 AS ?v) }"
            .to_string(),
        format!(
            "INSERT {{ <http://ex/x> <http://ex/t> ?v }} WHERE {{ BIND(STRDT(\"x\", <{XSD}int>) AS ?v) }}"
        ),
    ] {
        assert!(db.query(&q).is_err(), "{q}");
    }
    // Nothing of a refused operation is applied.
    let q = format!(
        "INSERT DATA {{ <http://ex/s> <http://ex/ok> 1 . <http://ex/s> <http://ex/p> \"y\"^^<{XSD}int> }}"
    );
    assert!(db.query(&q).is_err());
    assert!(all_keys(&mut db).is_empty());
    // Valid literals, ill-typed literals of unchecked types and big
    // decimals are stored as before.
    let q = format!(
        "INSERT DATA {{ <http://ex/s> <http://ex/p> 9223372036854775807, \"-12\"^^<{XSD}short>, \
         1.5e3, \"x\"^^<{XSD}decimal>, 99999999999999999999.5 }}"
    );
    db.query(&q).unwrap();
    assert_eq!(all_keys(&mut db).len(), 5);
}

fn run(bin: &str, args: &[&str], dir: &Path) -> std::process::Output {
    Command::new(bin)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

#[test]
fn gexport_then_gbuild_loses_nothing() {
    let tmp = TmpDir::new("export");
    std::fs::write(tmp.join("a.rdf"), rdfxml_doc("zh_CN")).unwrap();
    std::fs::write(
        tmp.join("b.jsonld"),
        "[{\"@id\": \"_:a b\", \"http://ex/p\": {\"@value\": \"v\", \"@language\": \"\"}}, \
         {\"@id\": \"_:a b\", \"http://ex/q\": \"w\"}]",
    )
    .unwrap();
    let mut db = Database::new("src");
    for f in ["a.rdf", "b.jsonld"] {
        db.query(&format!("LOAD <file://{}>", tmp.join(f).display()))
            .unwrap();
    }
    db.query(&format!(
        "INSERT DATA {{ <http://ex/s> <http://ex/n> 42, \"t\\u0001ab\\tx\" . \
         <http://ex/s> <http://ex/m> \"9\"^^<{XSD}byte> }}"
    ))
    .unwrap();
    // An IRI a lenient reader let in.
    assert!(db.insert_triple(&Triple::new(
        Term::iri("http://ex/a>b c"),
        Term::iri("http://ex/p"),
        Term::iri("http://ex/o"),
    )));
    let before = all_keys(&mut db);
    db.save(tmp.join("src.db")).unwrap();

    let out = run(env!("CARGO_BIN_EXE_gexport"), &["src", "out.nt"], &tmp.0);
    assert!(out.status.success(), "{out:?}");
    let out = run(env!("CARGO_BIN_EXE_gbuild"), &["dst", "out.nt"], &tmp.0);
    assert!(out.status.success(), "gbuild skipped statements: {out:?}");
    let mut rebuilt = Database::open(tmp.join("dst.db")).unwrap();
    let after = all_keys(&mut rebuilt);
    assert_eq!(after.len(), before.len(), "{before:?}\n{after:?}");
    // Blank nodes are renamed per import; everything else is identical.
    let no_blanks = |rows: &BTreeSet<Vec<String>>| -> BTreeSet<Vec<String>> {
        rows.iter()
            .map(|r| {
                r.iter()
                    .map(|k| {
                        if k.starts_with("_:") {
                            "_:".into()
                        } else {
                            k.clone()
                        }
                    })
                    .collect()
            })
            .collect()
    };
    assert_eq!(no_blanks(&after), no_blanks(&before));
}

#[test]
fn gbuild_exits_3_when_it_skipped_statements() {
    let tmp = TmpDir::new("gbuild");
    std::fs::write(
        tmp.join("d.nt"),
        format!(
            "<http://ex/s> <http://ex/p> <http://ex/o> .\n\
             <http://ex/s> <http://ex/p> \"x\"^^<{XSD}integer> .\n"
        ),
    )
    .unwrap();
    let out = run(env!("CARGO_BIN_EXE_gbuild"), &["d", "d.nt"], &tmp.0);
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("RDF parse error num 1"), "{stdout}");
    // The build finished all the same.
    let mut db = Database::open(tmp.join("d.db")).unwrap();
    assert_eq!(all_keys(&mut db).len(), 1);
    // A clean source exits 0.
    std::fs::write(
        tmp.join("e.nt"),
        "<http://ex/s> <http://ex/p> <http://ex/o> .\n",
    )
    .unwrap();
    let out = run(env!("CARGO_BIN_EXE_gbuild"), &["e", "e.nt"], &tmp.0);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
}

/// The RocksDB builds exit 3 as well: the bulk build and `--incremental`
/// (which returns through `build_rocksdb`) both report the skipped statement.
#[cfg(feature = "rocksdb")]
#[test]
fn gbuild_rocksdb_exits_3_when_it_skipped_statements() {
    let tmp = TmpDir::new("gbuild_rocks");
    std::fs::write(
        tmp.join("d.nt"),
        format!(
            "<http://ex/s> <http://ex/p> <http://ex/o> .\n\
             <http://ex/s> <http://ex/p> \"x\"^^<{XSD}integer> .\n"
        ),
    )
    .unwrap();
    for (name, flags) in [
        ("b", &["--rocksdb"][..]),
        ("i", &["--rocksdb", "--incremental"][..]),
    ] {
        let mut args = flags.to_vec();
        args.extend([name, "d.nt"]);
        let out = run(env!("CARGO_BIN_EXE_gbuild"), &args, &tmp.0);
        assert_eq!(out.status.code(), Some(3), "{flags:?}: {out:?}");
        let mut db = Database::open(tmp.join(&format!("{name}.db"))).unwrap();
        assert_eq!(all_keys(&mut db).len(), 1, "{flags:?}: the build finished");
    }
}

// ---- sparqlconf#11: IRI() / STRLANG() and N-Triples output -------------------

/// The review's injection IRI: `>` ends the term, the newline the statement.
const FORGED: &str = "http://a> <http://evil> <http://c> .\n<http://d";

#[test]
fn iri_and_strlang_refuse_text_that_is_no_term() {
    let mut db = Database::new("inj");
    let q = format!(
        "INSERT {{ ?s <http://ex/p> \"x\" }} WHERE {{ BIND(IRI(\"{}\") AS ?s) }}",
        nt_escape(FORGED)
    );
    db.query(&q).unwrap();
    let q = "INSERT { <http://ex/l> <http://ex/q> ?l } WHERE { BIND(STRLANG(\"x\", \"en> bad\") AS ?l) }";
    db.query(q).unwrap();
    let q = "INSERT { <http://ex/l> <http://ex/q> ?l } WHERE { BIND(STRLANG(\"x\", \"\") AS ?l) }";
    db.query(q).unwrap();
    assert!(
        all_keys(&mut db).is_empty(),
        "IRI()/STRLANG() built terms: {:?}",
        all_keys(&mut db)
    );
    // Valid input still works; URI() is IRI().
    db.query(
        "INSERT { ?s <http://ex/p> ?l } WHERE { BIND(URI(\"http://ex/ok\") AS ?s) \
         BIND(STRLANG(\"x\", \"zh-CN\") AS ?l) }",
    )
    .unwrap();
    assert_eq!(
        all_keys(&mut db),
        BTreeSet::from([vec![
            "<http://ex/ok>".to_string(),
            "<http://ex/p>".to_string(),
            "\"x\"@zh-CN".to_string()
        ]])
    );
    // A read-only CONSTRUCT that maps stored text to IRIs cannot forge
    // either (no write access needed for the injection before the fix).
    db.query(&format!(
        "INSERT DATA {{ <http://ex/u1> <http://ex/name> \"{}\" }}",
        nt_escape("bob> <http://ex/role> <http://ex/admin> .\n<http://ex/x")
    ))
    .unwrap();
    let r = db
        .query(
            "CONSTRUCT { ?u a <http://ex/User> } WHERE { ?x <http://ex/name> ?n \
             BIND(IRI(CONCAT(\"http://ex/user/\", ?n)) AS ?u) }",
        )
        .unwrap();
    let QueryResult::Construct(ts) = r else {
        panic!("{r:?}")
    };
    assert!(ts.is_empty(), "{ts:?}");
}

/// Send one request and read the whole response (`Connection: close`).
fn http(addr: SocketAddr, method: &str, target: &str, ctype: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    let raw = format!(
        "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: {ctype}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    s.write_all(raw.as_bytes()).unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).unwrap();
    let resp = String::from_utf8_lossy(&resp).into_owned();
    let (head, body) = resp.split_once("\r\n\r\n").unwrap_or((resp.as_str(), ""));
    let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, body.to_string())
}

/// A database holding terms an older version (or a lenient reader) let in.
fn odd_database() -> (Database, Vec<Triple>) {
    let mut db = Database::new("odd");
    let stored = vec![
        Triple::new(
            Term::iri(FORGED),
            Term::iri("http://ex/p"),
            lit("x", None, None),
        ),
        Triple::new(
            Term::iri("http://ex/l"),
            Term::iri("http://ex/q"),
            lit("x", Some("en> <http://evil> <http://c> .\n<http://d"), None),
        ),
        Triple::new(
            Term::blank("b <http://evil> <http://c> .\n_:d"),
            Term::iri("http://ex/r"),
            lit("v\u{1}", None, Some("http://ex/dt> . <x")),
        ),
    ];
    for t in &stored {
        assert!(db.insert_triple(t));
    }
    (db, stored)
}

#[test]
fn construct_over_http_cannot_forge_triples() {
    let (db, stored) = odd_database();
    let server = Arc::new(Server::bind(db, "127.0.0.1:0").unwrap());
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let worker = thread::spawn(move || s2.serve_forever());
    let (status, body) = http(
        addr,
        "POST",
        "/sparql",
        "application/sparql-query",
        "CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }",
    );
    server.shutdown();
    let _ = worker.join();
    assert_eq!(status, 200, "{body}");
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines.len(), stored.len(), "one line per triple: {body}");
    let parsed = ntriples::parse_str(&body).unwrap_or_else(|e| panic!("{e}: {body}"));
    assert_eq!(parsed.len(), stored.len(), "{body}");
    // The IRI and the datatype read back exactly; no statement mentions
    // the injected <http://evil> as a term of its own.
    assert!(parsed.contains(&stored[0]), "{parsed:?}");
    for t in &parsed {
        for term in [&t.subject, &t.predicate, &t.object] {
            assert_ne!(term, &Term::iri("http://evil"), "{body}");
        }
    }
}

#[test]
fn gquery_construct_output_cannot_forge_triples() {
    let tmp = TmpDir::new("gquery");
    let (db, stored) = odd_database();
    db.save(tmp.join("odd.db")).unwrap();
    let out = run(
        env!("CARGO_BIN_EXE_gquery"),
        &["odd", "-e", "CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }"],
        &tmp.0,
    );
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let parsed = ntriples::parse_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"));
    assert_eq!(parsed.len(), stored.len(), "{stdout}");
    assert!(parsed.contains(&stored[0]), "{parsed:?}");
}

// ---- query#15: result serializers --------------------------------------------

fn render(fmt: ResultFormat, rs: &gstore::ResultSet) -> String {
    let mut buf = Vec::new();
    sparql_results::write_select(fmt, rs, &mut buf).unwrap();
    String::from_utf8(buf).unwrap()
}

#[test]
fn result_serializers_render_cells_as_the_terms_they_are() {
    let mut db = Database::new("cells");
    assert!(db.insert_triple(&Triple::new(
        Term::iri("http://ex/a>b"),
        Term::iri("http://ex/p"),
        lit("a\u{1}b\rc", None, None),
    )));
    let r = db.query("SELECT ?s ?o WHERE { ?s ?p ?o }").unwrap();
    let QueryResult::Select(rs) = r else {
        panic!("{r:?}")
    };
    let json = render(ResultFormat::Json, &rs);
    assert!(
        json.contains(r#""s":{"type":"uri","value":"http://ex/a>b"}"#),
        "{json}"
    );
    assert!(json.contains(r#""value":"a\u0001b\rc""#), "{json}");
    let xml = render(ResultFormat::Xml, &rs);
    assert!(xml.contains("<uri>http://ex/a&gt;b</uri>"), "{xml}");
    assert!(
        !xml.chars().any(|c| c < ' ' && c != '\n' && c != '\t'),
        "XML 1.0 cannot hold a raw control character: {xml:?}"
    );
    assert!(xml.contains("<literal>a\u{FFFD}b&#xD;c</literal>"), "{xml}");
    let tsv = render(ResultFormat::Tsv, &rs);
    let row = tsv.lines().nth(1).unwrap();
    let cells: Vec<&str> = row.split('\t').collect();
    assert_eq!(cells.len(), 2, "{tsv:?}");
    assert_eq!(
        ntriples::parse_term(cells[0]).unwrap(),
        Term::iri("http://ex/a>b"),
        "{tsv:?}"
    );
    let csv = render(ResultFormat::Csv, &rs);
    assert!(
        csv.lines().nth(1).unwrap().starts_with("http://ex/a>b,"),
        "{csv}"
    );
    // Upstream's JSON shape too.
    let up = sparql_results::upstream_select_json(&rs, None).to_string();
    assert!(up.contains(r#""type":"uri""#), "{up}");
    // IRI() of such text is an error now; a stored one keeps its type.
    let cells = select_cells(&mut db, "SELECT ?s WHERE { ?s <http://ex/p> ?o }");
    assert_eq!(cells, vec![vec![Some("<http://ex/a>b>".to_string())]]);
}

// ---- sparqlconf#14: sameTerm agrees with the store's term identity -----------

#[test]
fn same_term_agrees_with_matching_joins_and_distinct() {
    let mut db = Database::build_from_str(
        "same",
        "@prefix : <http://ex/> . @prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n\
         :d :name \"Dave\"^^xsd:string . :e :name \"Dave\" . :e :label \"x\"@en . :f :label \"x\"@EN .",
    )
    .unwrap();
    let rows = |db: &mut Database, q: &str| select_cells(db, q).len();
    let ask = |db: &mut Database, q: &str| match db.query(q).unwrap() {
        QueryResult::Ask(b) => b,
        other => panic!("{other:?}"),
    };
    let x = "PREFIX : <http://ex/> PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> ";
    // The store keeps "Dave" and "Dave"^^xsd:string as two terms ...
    assert_eq!(
        rows(&mut db, &format!("{x} SELECT ?s {{ ?s :name \"Dave\" }}")),
        1
    );
    assert_eq!(
        rows(
            &mut db,
            &format!("{x} SELECT DISTINCT ?n {{ VALUES ?n {{ \"Dave\" \"Dave\"^^xsd:string }} }}")
        ),
        2
    );
    assert_eq!(
        rows(
            &mut db,
            &format!("{x} SELECT ?a ?b {{ ?a :name ?n . ?b :name ?n }}")
        ),
        2
    );
    // ... and so does sameTerm, in a constant and over the joined data.
    assert!(!ask(
        &mut db,
        &format!("{x} ASK {{ FILTER(sameTerm(\"abc\"^^xsd:string, \"abc\")) }}")
    ));
    assert_eq!(
        rows(
            &mut db,
            &format!("{x} SELECT ?a ?b {{ ?a :name ?m . ?b :name ?n FILTER(sameTerm(?m, ?n)) }}")
        ),
        2
    );
    // Language tags: compared as written, as matching does.
    assert_eq!(
        rows(&mut db, &format!("{x} SELECT ?s {{ ?s :label \"x\"@en }}")),
        1
    );
    assert!(!ask(
        &mut db,
        &format!("{x} ASK {{ FILTER(sameTerm(\"x\"@en, \"x\"@EN)) }}")
    ));
    assert!(ask(
        &mut db,
        &format!("{x} ASK {{ FILTER(sameTerm(\"x\"@en, \"x\"@en)) }}")
    ));
}
