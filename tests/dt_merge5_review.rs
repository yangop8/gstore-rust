//! Regressions from the review of the Phase 5 merge (streaming import, named
//! graphs, blank-node scoping) into Phases 0-3 (atomic `LOAD`, graph
//! management, servers that save after a write):
//!
//! * a failed `LOAD` that applied nothing is no change: the counters the
//!   servers read do not move, the CSR survives, nothing is saved, and the
//!   error does not claim a partial update; nor does it leave an empty
//!   named graph behind;
//! * an empty graph IRI (`<>` without a `BASE`) names a graph — neither
//!   `NAMED` (every named graph) in `DROP` / `CLEAR`, nor the default graph
//!   in the redo log;
//! * terms the lenient readers accept (an IRI holding `>` or `\`) survive
//!   `MOVE` / `COPY` / `ADD`, `DELETE … WHERE`, `DESCRIBE` and redo replay;
//!   an empty language tag is a syntax error, as in N-Triples;
//! * a blank node naming a graph keeps co-referring with the same blank node
//!   as a subject or object;
//! * what an import holds in memory, and what it writes to
//!   `parse_error.log`, is bounded by more than the input's size: long
//!   expanded IRIs (prefixes, `xml:base`) cannot multiply it;
//! * `remote=true` downloads do not reach host-local addresses unless the
//!   load policy allows it;
//! * the server's own export of a database with named graphs (`.nq`) can be
//!   built again under the default `upload_allow_extensions`.

#[path = "upstream_scripts/harness.rs"]
mod harness;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::sync::Arc;
use std::thread;

use gstore::parser::import::{self, ImportOptions, ParseErrorLog, RdfFormat};
use gstore::parser::nquads::Quad;
use gstore::server::Server as SingleServer;
use gstore::{Database, GStoreError, LoadPolicy, QueryResult, Term};
use harness::*;

// ---- helpers ----------------------------------------------------------------

fn write(dir: &Path, name: &str, text: &str) -> String {
    let p = dir.join(name);
    fs::write(&p, text).unwrap();
    p.to_string_lossy().into_owned()
}

/// Sorted rows of a SELECT, cells joined by a space (unbound: `-`).
fn rows(db: &mut Database, q: &str) -> Vec<String> {
    let rs = db.select(q).unwrap_or_else(|e| panic!("{q}: {e}"));
    let mut v: Vec<String> = rs
        .rows
        .into_iter()
        .map(|r| {
            r.into_iter()
                .map(|c| c.unwrap_or_else(|| "-".into()))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    v.sort();
    v
}

fn update(db: &mut Database, q: &str) -> usize {
    match db.query(q).unwrap_or_else(|e| panic!("{q}: {e}")) {
        QueryResult::Update { changed } => changed,
        other => panic!("{q}: {other:?}"),
    }
}

fn ask(db: &mut Database, q: &str) -> bool {
    match db.query(q).unwrap_or_else(|e| panic!("{q}: {e}")) {
        QueryResult::Ask(b) => b,
        other => panic!("{q}: {other:?}"),
    }
}

/// Every statement: `graph s p o` (default graph: `-`).
fn dump(db: &mut Database) -> Vec<String> {
    let mut v = rows(db, "SELECT ?s ?p ?o WHERE { ?s ?p ?o }")
        .into_iter()
        .map(|r| format!("- {r}"))
        .collect::<Vec<_>>();
    v.extend(rows(
        db,
        "SELECT ?g ?s ?p ?o WHERE { GRAPH ?g { ?s ?p ?o } }",
    ));
    v.sort();
    v
}

/// More good N-Triples than one import batch holds, then a broken line: the
/// first batch is inserted before the error is read.
fn big_bad_nt(dir: &Path) -> String {
    let mut text = String::new();
    for i in 0..(import::DEFAULT_BATCH_SIZE + 1000) {
        text.push_str(&format!("<http://ex/s{i}> <http://ex/p> \"v{i}\" .\n"));
    }
    text.push_str("<http://ex/bad> <http://ex/p> \"unterminated .\n");
    write(dir, "big_bad.nt", &text)
}

/// The heap bytes the strings of a quad hold.
fn quad_bytes(q: &Quad) -> usize {
    fn term(t: &Term) -> usize {
        match t {
            Term::Iri(s) | Term::Blank(s) => s.len(),
            Term::Literal {
                value,
                datatype,
                lang,
            } => {
                value.len()
                    + datatype.as_ref().map_or(0, String::len)
                    + lang.as_ref().map_or(0, String::len)
            }
        }
    }
    term(&q.subject) + term(&q.predicate) + term(&q.object) + q.graph.as_ref().map_or(0, term)
}

/// An HTTP server answering `path` with `body` (404 otherwise), counting
/// the connections it accepts.
fn http_server(
    path: &'static str,
    body: Vec<u8>,
) -> (SocketAddr, Arc<std::sync::atomic::AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let h2 = Arc::clone(&hits);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            h2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let mut reader = BufReader::new(s.try_clone().unwrap());
            let mut first = String::new();
            if reader.read_line(&mut first).is_err() {
                continue;
            }
            loop {
                let mut l = String::new();
                if reader.read_line(&mut l).unwrap_or(0) == 0 || l == "\r\n" {
                    break;
                }
            }
            let found = first.split_whitespace().nth(1) == Some(path);
            let payload: &[u8] = if found { &body } else { b"" };
            let status = if found { "200 OK" } else { "404 Not Found" };
            let _ = s.write_all(
                format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                )
                .as_bytes(),
            );
            let _ = s.write_all(payload);
        }
    });
    (addr, hits)
}

// ---- a failed LOAD is no change ---------------------------------------------

#[test]
fn a_failed_load_that_applied_nothing_is_no_change() {
    let dir = scratch("m5_load_nochange");
    let bad = big_bad_nt(&dir);
    let mut db =
        Database::build_from_str("a", "<http://ex/k> <http://ex/p> <http://ex/k> .\n").unwrap();
    db.preload_csr().unwrap();
    let before = db.mutation_count();
    let statements = dump(&mut db);
    for q in [
        format!("LOAD <{bad}>"),
        format!("LOAD <{bad}> INTO GRAPH <http://ex/t>"),
        format!("LOAD SILENT <{bad}>"),
        format!("LOAD SILENT <{bad}> INTO GRAPH <http://ex/t>"),
    ] {
        let _ = db.query(&q);
        assert_eq!(dump(&mut db), statements, "{q}");
        assert_eq!(
            db.mutation_count(),
            before,
            "{q}: nothing changed, so the change counter must not move"
        );
        assert!(db.csr_is_cached(), "{q}: the CSR of unchanged data stays");
        // No empty named graph is left behind either.
        assert!(!ask(&mut db, "ASK { GRAPH <http://ex/t> {} }"), "{q}");
        assert_eq!(
            rows(&mut db, "SELECT DISTINCT ?g WHERE { GRAPH ?g {} }"),
            Vec::<String>::new(),
            "{q}"
        );
    }
    // A LOAD that applied something does count.
    let good = write(
        &dir,
        "good.nt",
        "<http://ex/n> <http://ex/p> <http://ex/m> .\n",
    );
    update(&mut db, &format!("LOAD <{good}>"));
    assert_ne!(db.mutation_count(), before);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_named_graph_whose_last_statement_goes_is_gone() {
    let mut db = Database::new("e");
    update(
        &mut db,
        "INSERT DATA { GRAPH <http://ex/z> { <http://ex/a> <http://ex/p> <http://ex/b> } }",
    );
    update(
        &mut db,
        "DELETE DATA { GRAPH <http://ex/z> { <http://ex/a> <http://ex/p> <http://ex/b> } }",
    );
    assert!(!ask(&mut db, "ASK { GRAPH <http://ex/z> {} }"));
    // An insert rolled back with its transaction leaves no graph either.
    db.begin().unwrap();
    update(
        &mut db,
        "INSERT DATA { GRAPH <http://ex/y> { <http://ex/a> <http://ex/p> <http://ex/b> } }",
    );
    db.rollback().unwrap();
    assert!(!ask(&mut db, "ASK { GRAPH <http://ex/y> {} }"));
    assert_eq!(
        rows(&mut db, "SELECT DISTINCT ?g WHERE { GRAPH ?g {} }"),
        Vec::<String>::new()
    );
}

/// The single-database server (`gserver DIR`) reports a failed `LOAD` that
/// applied nothing as a plain error and does not save.
#[test]
fn the_single_database_server_does_not_save_or_blame_a_failed_load() {
    let dir = scratch("m5_single_load");
    let bad = big_bad_nt(&dir);
    let db_dir = dir.join("db1");
    let server = SingleServer::bind(Database::new("db1"), "127.0.0.1:0")
        .unwrap()
        .with_check_point(true)
        .with_load_policy(LoadPolicy::restricted().with_allowed_dir(&dir))
        .with_db_dir(&db_dir);
    let server = Arc::new(server);
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    let serving = thread::spawn(move || s2.serve_forever());
    let (status, body) = http(&addr, "POST", "/update", format!("LOAD <{bad}>").as_bytes());
    assert_ne!(status, 200, "{body}");
    assert!(body.contains("unterminated"), "{body}");
    assert!(
        !body.contains("were applied"),
        "nothing was applied: {body}"
    );
    let (status, body) = http(
        &addr,
        "POST",
        "/update",
        format!("LOAD SILENT <{bad}>").as_bytes(),
    );
    assert_eq!(status, 200, "{body}");
    assert!(!db_dir.exists(), "nothing changed, so nothing is saved");
    server.shutdown();
    serving.join().unwrap();
    let _ = fs::remove_dir_all(&dir);
}

// ---- an empty graph IRI is a graph ------------------------------------------

#[test]
fn drop_or_clear_of_the_empty_graph_iri_is_not_drop_named() {
    let mut db = Database::new("e");
    update(
        &mut db,
        "INSERT DATA { GRAPH <http://ex/keep1> { <http://ex/a> <http://ex/p> <http://ex/b> } \
         GRAPH <http://ex/keep2> { <http://ex/a> <http://ex/p> <http://ex/c> } }",
    );
    let all = dump(&mut db);
    assert_eq!(update(&mut db, "DROP GRAPH <>"), 0);
    assert_eq!(update(&mut db, "CLEAR GRAPH <>"), 0);
    assert_eq!(update(&mut db, "DROP SILENT GRAPH <>"), 0);
    assert_eq!(dump(&mut db), all, "no named graph is <>");
    // The NAMED keyword still means every named graph.
    assert_eq!(update(&mut db, "CLEAR NAMED"), 2);
    assert_eq!(dump(&mut db), Vec::<String>::new());

    // A document's `<>` graph is a graph of its own.
    let dir = scratch("m5_empty_iri");
    let nq = write(
        &dir,
        "e.nq",
        "<http://ex/s> <http://ex/p> <http://ex/o1> <> .\n\
         <http://ex/s> <http://ex/p> <http://ex/o2> <http://ex/g2> .\n",
    );
    let mut db = Database::new("e");
    update(&mut db, &format!("LOAD <{nq}>"));
    assert_eq!(update(&mut db, "CLEAR GRAPH <>"), 1);
    assert_eq!(
        dump(&mut db),
        ["<http://ex/g2> <http://ex/s> <http://ex/p> <http://ex/o2>"],
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn the_redo_log_keeps_the_empty_graph_iri_apart_from_the_default_graph() {
    let dir = scratch("m5_redo_empty");
    let nq = write(
        &dir,
        "e.nq",
        "<http://ex/s> <http://ex/p> <http://ex/o1> <> .\n\
         <http://ex/s> <http://ex/p> <http://ex/o2> <http://ex/g2> .\n",
    );
    let logs = dir.join("logs");
    fs::create_dir_all(&logs).unwrap();
    let mut db = Database::new("r");
    db.enable_redo_log_in(&logs).unwrap();
    update(
        &mut db,
        "INSERT DATA { <http://ex/s> <http://ex/p> <http://ex/o1> }",
    );
    update(&mut db, &format!("LOAD <{nq}>"));
    update(
        &mut db,
        "INSERT DATA { GRAPH <> { <http://ex/s> <http://ex/p> <http://ex/o3> } }",
    );
    update(
        &mut db,
        "DELETE DATA { GRAPH <> { <http://ex/s> <http://ex/p> <http://ex/o1> } }",
    );
    let want = dump(&mut db);
    assert!(
        want.contains(&"- <http://ex/s> <http://ex/p> <http://ex/o1>".to_string()),
        "{want:?}"
    );
    assert!(
        want.contains(&"<> <http://ex/s> <http://ex/p> <http://ex/o3>".to_string()),
        "{want:?}"
    );
    let mut fresh = Database::new("r2");
    fresh.replay_redo_log(logs.join("redo.log")).unwrap();
    assert_eq!(dump(&mut fresh), want);
    let _ = fs::remove_dir_all(&dir);
}

// ---- terms the lenient readers accept ----------------------------------------

#[test]
fn iris_with_escaped_delimiters_survive_graph_management_updates_and_redo() {
    let dir = scratch("m5_weird_iri");
    // `>` is `>`, `\` is `\`: the readers decode them into the IRI.
    let nt = write(
        &dir,
        "weird.nt",
        "<http://ex/a\\u003Eb> <http://ex/p> <http://ex/c\\u005Cd> .\n\
         <http://ex/n> <http://ex/p> <http://ex/m> .\n",
    );
    let mut db = Database::new("w");
    assert_eq!(
        update(&mut db, &format!("LOAD <{nt}> INTO GRAPH <http://ex/g>")),
        2
    );
    let odd = "SELECT ?s ?o WHERE { ?s <http://ex/p> ?o FILTER(CONTAINS(STR(?s), \">\")) }";
    // MOVE / COPY / ADD carry it.
    update(&mut db, "MOVE GRAPH <http://ex/g> TO DEFAULT");
    assert_eq!(rows(&mut db, odd).len(), 1, "MOVE lost the statement");
    update(&mut db, "COPY DEFAULT TO GRAPH <http://ex/h>");
    update(&mut db, "ADD DEFAULT TO GRAPH <http://ex/i>");
    for g in ["h", "i"] {
        let n = rows(
            &mut db,
            &format!("SELECT ?s WHERE {{ GRAPH <http://ex/{g}> {{ ?s ?p ?o }} }}"),
        )
        .len();
        assert_eq!(n, 2, "graph {g}");
    }
    // DESCRIBE shows it.
    match db
        .query("DESCRIBE ?s WHERE { ?s <http://ex/p> ?o FILTER(CONTAINS(STR(?s), \">\")) }")
        .unwrap()
    {
        QueryResult::Construct(t) => assert_eq!(t.len(), 1, "{t:?}"),
        other => panic!("{other:?}"),
    }
    // DELETE / INSERT … WHERE see it.
    assert_eq!(
        update(
            &mut db,
            "DELETE { ?s <http://ex/p> ?o } INSERT { ?s <http://ex/q> ?o } WHERE { ?s <http://ex/p> ?o }"
        ),
        4
    );
    assert_eq!(update(&mut db, "DELETE WHERE { ?s <http://ex/q> ?o }"), 2);
    assert_eq!(dump(&mut db).len(), 4);

    // The redo log replays it.
    let logs = dir.join("logs");
    fs::create_dir_all(&logs).unwrap();
    let mut db = Database::new("w");
    db.enable_redo_log_in(&logs).unwrap();
    update(&mut db, &format!("LOAD <{nt}>"));
    update(
        &mut db,
        "DELETE { ?s <http://ex/p> ?o } INSERT { GRAPH <http://ex/g> { ?s <http://ex/q> ?o } } \
         WHERE { ?s <http://ex/p> ?o FILTER(CONTAINS(STR(?s), \">\")) }",
    );
    let want = dump(&mut db);
    assert_eq!(
        want,
        [
            "- <http://ex/n> <http://ex/p> <http://ex/m>",
            "<http://ex/g> <http://ex/a>b> <http://ex/q> <http://ex/c\\d>",
        ]
    );
    let mut fresh = Database::new("w2");
    fresh.replay_redo_log(logs.join("redo.log")).unwrap();
    assert_eq!(dump(&mut fresh), want);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_empty_language_tag_is_a_syntax_error() {
    let dir = scratch("m5_empty_lang");
    let nt = write(
        &dir,
        "lang.nt",
        "<http://ex/s> <http://ex/p> \"x\"@ .\n<http://ex/s2> <http://ex/p> \"y\" .\n",
    );
    let mut db = Database::new("l");
    let err = db.query(&format!("LOAD <{nt}>")).unwrap_err();
    assert!(matches!(err, GStoreError::RdfParse { .. }), "{err}");
    let mut failures: Vec<(String, gstore::parser::stream::SyntaxError)> = Vec::new();
    let report = import::import_reader(
        fs::read(&nt).unwrap().as_slice(),
        RdfFormat::Turtle,
        "lang.nt",
        &ImportOptions::tolerant(),
        &mut failures,
        &mut |b: &mut Vec<Quad>| {
            b.clear();
            Ok(())
        },
    )
    .unwrap();
    assert_eq!((report.statements, report.failed), (1, 1), "{failures:?}");
    let _ = fs::remove_dir_all(&dir);
}

// ---- blank graph names -------------------------------------------------------

#[test]
fn a_blank_graph_name_co_refers_with_the_same_blank_node() {
    let dir = scratch("m5_bnode_graph");
    let join = "SELECT ?src WHERE { GRAPH ?g { <http://ex/a> <http://ex/b> <http://ex/c> } \
                ?g <http://ex/source> ?src }";
    for (name, text) in [
        (
            "prov.trig",
            "_:g { <http://ex/a> <http://ex/b> <http://ex/c> . }\n\
             _:g <http://ex/source> <http://ex/s1> .\n",
        ),
        (
            "prov.nq",
            "<http://ex/a> <http://ex/b> <http://ex/c> _:g .\n\
             _:g <http://ex/source> <http://ex/s1> .\n",
        ),
    ] {
        let f = write(&dir, name, text);
        let mut db = Database::new("b");
        update(&mut db, &format!("LOAD <{f}>"));
        assert_eq!(rows(&mut db, join), ["<http://ex/s1>"], "{name}");
    }
    // Used as a term before it names a graph: every use as a term stays one
    // node (the graph is apart from it, as before), never split in two.
    let f = write(
        &dir,
        "rev.trig",
        "_:n <http://ex/source> <http://ex/s1> .\n\
         _:n { <http://ex/a> <http://ex/b> <http://ex/c> . }\n\
         _:n <http://ex/created> \"x\" .\n",
    );
    let mut db = Database::new("b");
    update(&mut db, &format!("LOAD <{f}>"));
    assert_eq!(
        rows(
            &mut db,
            "SELECT ?src ?c WHERE { ?n <http://ex/source> ?src . ?n <http://ex/created> ?c }"
        ),
        ["<http://ex/s1> \"x\""]
    );
    let _ = fs::remove_dir_all(&dir);
}

// ---- bounded import memory and parse_error.log --------------------------------

fn long_prefix(n: usize) -> String {
    format!("http://ex/{}#", "x".repeat(n))
}

#[test]
fn an_import_batch_is_bounded_in_bytes_not_only_in_statements() {
    // 300 short lines, each a quad of three 100 KB IRIs: 90 MB of quads.
    let mut text = format!("@prefix p: <{}> .\n", long_prefix(100_000));
    for _ in 0..300 {
        text.push_str("p:a p:b p:c .\n");
    }
    let mut batches = 0;
    let mut largest = 0;
    let mut total = 0;
    import::import_reader(
        text.as_bytes(),
        RdfFormat::Turtle,
        "t.ttl",
        &ImportOptions::strict(),
        &mut (),
        &mut |b: &mut Vec<Quad>| {
            batches += 1;
            total += b.len();
            largest = largest.max(b.iter().map(quad_bytes).sum::<usize>());
            b.clear();
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(total, 300);
    assert!(batches >= 2, "{batches} batch(es)");
    assert!(
        largest <= (64 << 20) + 400_000,
        "a batch held {largest} bytes"
    );
    assert_eq!(
        ImportOptions::strict().batch_bytes,
        import::DEFAULT_BATCH_BYTES
    );

    // The bound is an option.
    let opts = ImportOptions {
        batch_bytes: 4 << 20,
        ..ImportOptions::strict()
    };
    let (mut batches, mut largest) = (0, 0);
    import::import_reader(
        text.as_bytes(),
        RdfFormat::Turtle,
        "t.ttl",
        &opts,
        &mut (),
        &mut |b: &mut Vec<Quad>| {
            batches += 1;
            largest = largest.max(b.iter().map(quad_bytes).sum::<usize>());
            b.clear();
            Ok(())
        },
    )
    .unwrap();
    assert!(batches >= 20, "{batches} batch(es)");
    assert!(
        largest <= (4 << 20) + 400_000,
        "a batch held {largest} bytes"
    );
}

#[test]
fn a_statement_or_document_that_expands_without_bound_fails() {
    // One Turtle statement: 72 KB of input, an object list whose quads would
    // hold 3 × 100 KB each, 120 MB in all.
    let mut text = format!("@prefix p: <{}> .\np:s p:p p:o", long_prefix(100_000));
    for _ in 0..400 {
        text.push_str(", p:o");
    }
    text.push_str(" .\n");
    let err = import::import_reader(
        text.as_bytes(),
        RdfFormat::Turtle,
        "big.ttl",
        &ImportOptions::strict(),
        &mut (),
        &mut |b: &mut Vec<Quad>| {
            b.clear();
            Ok(())
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("expands to more than"), "{err}");

    // RDF/XML, parsed whole: a long namespace and base, 3000 descriptions.
    let ns = long_prefix(20_000);
    let base = format!("http://ex/{}/", "x".repeat(20_000));
    let mut xml = format!(
        "<?xml version=\"1.0\"?>\n<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" \
         xmlns:p=\"{ns}\" xml:base=\"{base}\">\n"
    );
    for i in 0..3000 {
        xml.push_str(&format!(
            "<rdf:Description rdf:about=\"s{i}\"><p:b>x</p:b></rdf:Description>\n"
        ));
    }
    xml.push_str("</rdf:RDF>\n");
    let mut failures: Vec<(String, gstore::parser::stream::SyntaxError)> = Vec::new();
    let report = import::import_reader(
        xml.as_bytes(),
        RdfFormat::RdfXml,
        "big.rdf",
        &ImportOptions::tolerant(),
        &mut failures,
        &mut |b: &mut Vec<Quad>| {
            b.clear();
            Ok(())
        },
    )
    .unwrap();
    assert_eq!((report.statements, report.failed), (0, 1), "{failures:?}");
    assert!(
        failures[0].1.message.contains("expands to more than"),
        "{failures:?}"
    );
}

#[test]
fn parse_error_log_lines_are_bounded() {
    let dir = scratch("m5_parse_log");
    let mut text = format!(
        "@prefix p: <{}> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n",
        long_prefix(100_000)
    );
    for _ in 0..100 {
        text.push_str("p:a p:b !\n");
    }
    for _ in 0..100 {
        text.push_str("p:s p:p \"x\"^^xsd:int .\n");
    }
    let path = dir.join("parse_error.log");
    let mut log = ParseErrorLog::open(&path).unwrap();
    let report = import::import_reader(
        text.as_bytes(),
        RdfFormat::Turtle,
        "t.ttl",
        &ImportOptions::tolerant(),
        &mut log,
        &mut |b: &mut Vec<Quad>| {
            b.clear();
            Ok(())
        },
    )
    .unwrap();
    drop(log);
    assert_eq!(report.failed, 200);
    let size = fs::metadata(&path).unwrap().len();
    assert!(size < 200 * 1024, "parse_error.log grew to {size} bytes");
    let text = fs::read_to_string(&path).unwrap();
    assert_eq!(
        import::count_logged_failures(&path),
        200,
        "one line per failure"
    );
    assert!(text.lines().all(|l| l.len() < 1024), "a line over 1 KiB");
    let _ = fs::remove_dir_all(&dir);
}

// ---- remote downloads ------------------------------------------------------------

#[test]
fn remote_downloads_do_not_reach_host_local_addresses_by_default() {
    let (http_addr, hits) = http_server(
        "/d.nt",
        b"<http://ex/r> <http://ex/p> <http://ex/o> .\n".to_vec(),
    );
    let root = scratch("m5_remote_local");
    let mut config = gstore::config::GStoreConfig::default();
    config.ip_deny_path = None;
    config.thread_num = 4;
    // What `gserver --allow-service` sets up.
    let mut policy = LoadPolicy::restricted();
    policy.allow_remote = true;
    let server = Arc::new(
        gstore::http_api::ApiServer::bind(&root, PASSWORD, "127.0.0.1:0")
            .unwrap()
            .with_config(config)
            .with_load_policy(policy),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    let op = |operation: &str, params: &[(&str, &str)]| {
        let mut body = gstore::json::Json::obj()
            .with("operation", operation)
            .with("username", USERNAME)
            .with("password", PASSWORD);
        for (k, v) in params {
            body.set(*k, *v);
        }
        let (_, text) = http(&addr, "POST", "/api", body.to_string().as_bytes());
        gstore::json::parse(&text).unwrap()
    };
    ok(&op("build", &[("db_name", "rdb")]));
    ok(&op("load", &[("db_name", "rdb")]));
    let url = format!("http://{http_addr}/d.nt");
    let j = op(
        "batchInsert",
        &[("db_name", "rdb"), ("file", &url), ("remote", "true")],
    );
    assert_eq!(code(&j), 1005, "{j}");
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no connection may reach the loopback host"
    );
    server.shutdown();
    let _ = fs::remove_dir_all(&root);

    // A policy that allows host-local addresses downloads it.
    let root = scratch("m5_remote_local_ok");
    let mut config = gstore::config::GStoreConfig::default();
    config.ip_deny_path = None;
    config.thread_num = 4;
    let mut policy = LoadPolicy::restricted();
    policy.allow_remote = true;
    policy.allow_local_hosts = true;
    let server = Arc::new(
        gstore::http_api::ApiServer::bind(&root, PASSWORD, "127.0.0.1:0")
            .unwrap()
            .with_config(config)
            .with_load_policy(policy),
    );
    let addr = server.local_addr().unwrap();
    let s2 = Arc::clone(&server);
    thread::spawn(move || s2.serve_forever());
    let op = |operation: &str, params: &[(&str, &str)]| {
        let mut body = gstore::json::Json::obj()
            .with("operation", operation)
            .with("username", USERNAME)
            .with("password", PASSWORD);
        for (k, v) in params {
            body.set(*k, *v);
        }
        let (_, text) = http(&addr, "POST", "/api", body.to_string().as_bytes());
        gstore::json::parse(&text).unwrap()
    };
    ok(&op("build", &[("db_name", "rdb")]));
    ok(&op("load", &[("db_name", "rdb")]));
    let j = op(
        "batchInsert",
        &[("db_name", "rdb"), ("file", &url), ("remote", "true")],
    );
    ok(&j);
    assert_eq!(int(&j, "success_num"), 1, "{j}");
    server.shutdown();
    let _ = fs::remove_dir_all(&root);
}

// ---- export, then build again ----------------------------------------------------

#[test]
fn the_export_of_a_database_with_named_graphs_builds_again_by_default() {
    let srv = Server::start("m5_export_nq");
    let src = srv.root.join("upload");
    fs::create_dir_all(&src).unwrap();
    fs::write(
        src.join("d.nt"),
        "<http://e/a> <http://e/p> <http://e/b> .\n",
    )
    .unwrap();
    ok(&srv.op("build", &[("db_name", "demo"), ("db_path", "d.nt")]));
    srv.load("demo");
    ok(&srv.query(
        "demo",
        "INSERT DATA { GRAPH <http://e/g> { <http://e/c> <http://e/p> <http://e/d> } }",
    ));
    let j = srv.op("export", &[("db_name", "demo")]);
    ok(&j);
    let path = string(&j, "filepath");
    assert!(path.ends_with(".nq"), "{path}");
    let j = srv.op("build", &[("db_name", "copy"), ("db_path", &path)]);
    ok(&j);
    srv.load("copy");
    let j = srv.query(
        "copy",
        "SELECT ?o WHERE { GRAPH <http://e/g> { ?s ?p ?o } }",
    );
    ok(&j);
    assert_eq!(answer_table(&j).rows.len(), 1, "{j}");
}

#[cfg(feature = "gzip")]
#[test]
fn a_gzip_source_passes_the_extension_check_by_its_inner_extension() {
    use std::io::Write as _;
    let srv = Server::start("m5_gz");
    let up = srv.root.join("upload");
    fs::create_dir_all(&up).unwrap();
    let gz = |name: &str| {
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(b"<http://e/a> <http://e/p> <http://e/b> .\n")
            .unwrap();
        fs::write(up.join(name), enc.finish().unwrap()).unwrap();
    };
    gz("d.nt.gz");
    gz("d.csv.gz");
    ok(&srv.op("build", &[("db_name", "gzdb"), ("db_path", "d.nt.gz")]));
    let j = srv.op("build", &[("db_name", "gzdb2"), ("db_path", "d.csv.gz")]);
    assert_eq!(code(&j), 1003, "{j}");
}
