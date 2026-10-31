//! SPARQL `LOAD` through the bulk loaders' import pipeline (Phase 3's
//! `LOAD [SILENT] <src> [INTO GRAPH <g>]` meets Phase 5's streaming import),
//! and the Phase 3 graph-management updates on what that import produces.
//!
//! * **Pipeline.** The format follows the file extension (N-Triples, Turtle,
//!   N-Quads, TriG, RDF/XML); the blank nodes of every `LOAD` get their own
//!   scope; the [`LoadPolicy`] still confines local files.
//! * **Datasets.** `LOAD <x.trig>` / `<x.nq>` keeps the document's named
//!   graphs. `LOAD … INTO GRAPH <g>` puts the document's default graph into
//!   `<g>` and changes no other graph: the statements of the document's own
//!   named graphs are not loaded (SPARQL 1.1 Update §3.1.4; Jena does the
//!   same).
//! * **Strict and atomic.** A statement that does not parse fails the
//!   `LOAD` and nothing of it stays — `SILENT` only hides the error. A
//!   cancelled `LOAD` applies nothing either, `SILENT` or not.
//! * **Graph management.** `ADD` / `MOVE` / `COPY`, `GRAPH` patterns and
//!   `WITH` / `USING` updates work on graphs that came from an N-Quads / TriG
//!   import, blank nodes included, on the in-memory, on-disk and (feature
//!   `rocksdb`) RocksDB backends, and survive a reopen.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use gstore::{Database, GStoreError, LoadPolicy, QueryResult};

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gstore_dt_load_{tag}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

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

fn count(db: &mut Database, q: &str) -> usize {
    db.select(q).unwrap().rows.len()
}

fn update(db: &mut Database, q: &str) -> usize {
    match db.query(q).unwrap_or_else(|e| panic!("{q}: {e}")) {
        QueryResult::Update { changed } => changed,
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

const TRIG: &str = "@prefix ex: <http://ex/> .\n\
                    ex:d1 ex:p ex:d2 .\n\
                    ex:g1 { ex:a ex:p ex:b . _:x ex:name \"x\" . ex:a ex:knows _:x . }\n\
                    { ex:d3 ex:p ex:d4 . }\n\
                    GRAPH ex:g2 { ex:c ex:p ex:d . }\n";

const NQ: &str = "<http://ex/d1> <http://ex/p> <http://ex/d2> .\n\
                  <http://ex/a> <http://ex/p> <http://ex/b> <http://ex/g1> .\n\
                  _:x <http://ex/name> \"x\" <http://ex/g1> .\n\
                  <http://ex/a> <http://ex/knows> _:x <http://ex/g1> .\n\
                  <http://ex/c> <http://ex/p> <http://ex/d> <http://ex/g2> .\n";

#[test]
fn load_keeps_a_datasets_named_graphs_and_into_loads_only_its_default_graph() {
    let dir = tmp("dataset");
    for (name, text, default_n) in [("data.trig", TRIG, 2), ("data.nq", NQ, 1)] {
        let file = write(&dir, name, text);
        // Without INTO: a dataset load.
        let mut db = Database::new("ds");
        let n = update(&mut db, &format!("LOAD <file://{file}>"));
        assert_eq!(n, default_n + 4, "{name}");
        assert_eq!(db.triple_num() as usize, default_n, "{name}");
        assert_eq!(
            rows(
                &mut db,
                "SELECT ?g (COUNT(*) AS ?n) WHERE { GRAPH ?g { ?s ?p ?o } } GROUP BY ?g"
            ),
            [
                "<http://ex/g1> \"3\"^^<http://www.w3.org/2001/XMLSchema#integer>",
                "<http://ex/g2> \"1\"^^<http://www.w3.org/2001/XMLSchema#integer>"
            ],
            "{name}"
        );
        // The blank node is one node within its graph.
        assert_eq!(
            rows(
                &mut db,
                "SELECT ?n WHERE { GRAPH <http://ex/g1> { <http://ex/a> <http://ex/knows> ?b . \
                 ?b <http://ex/name> ?n } }"
            ),
            ["\"x\""],
            "{name}"
        );

        // INTO GRAPH: the document's default graph goes to <t>; its own
        // named graphs are not loaded and no other graph changes.
        let mut db = Database::new("into");
        update(
            &mut db,
            "INSERT DATA { GRAPH <http://ex/g1> { <http://ex/k> <http://ex/p> <http://ex/k> } }",
        );
        let before_g1 = count(
            &mut db,
            "SELECT * WHERE { GRAPH <http://ex/g1> { ?s ?p ?o } }",
        );
        let n = update(
            &mut db,
            &format!("LOAD <file://{file}> INTO GRAPH <http://ex/t>"),
        );
        assert_eq!(n, default_n, "{name}: only the default graph's statements");
        assert_eq!(db.triple_num(), 0, "{name}: the default graph is untouched");
        assert_eq!(
            count(
                &mut db,
                "SELECT * WHERE { GRAPH <http://ex/t> { ?s ?p ?o } }"
            ),
            default_n,
            "{name}"
        );
        assert_eq!(
            count(
                &mut db,
                "SELECT * WHERE { GRAPH <http://ex/g1> { ?s ?p ?o } }"
            ),
            before_g1,
            "{name}: <g1> is not changed by LOAD INTO <t>"
        );
        assert_eq!(
            count(
                &mut db,
                "SELECT * WHERE { GRAPH <http://ex/g2> { ?s ?p ?o } }"
            ),
            0,
            "{name}: <g2> is not created by LOAD INTO <t>"
        );
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn load_reads_every_format_by_extension_and_scopes_blank_nodes_per_load() {
    let dir = tmp("formats");
    let nt = write(&dir, "a.nt", "<http://ex/s> <http://ex/p> \"nt\" .\n");
    let ttl = write(
        &dir,
        "b.ttl",
        "@prefix ex: <http://ex/> . ex:s ex:p \"ttl\" ; ex:q [ ex:r 1 ] .\n",
    );
    let rdf = write(
        &dir,
        "c.rdf",
        "<?xml version=\"1.0\"?>\n<rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\" \
         xmlns:ex=\"http://ex/\">\n<rdf:Description rdf:about=\"http://ex/s\"><ex:p>rdf</ex:p>\
         </rdf:Description>\n</rdf:RDF>\n",
    );
    let nq = write(
        &dir,
        "d.nq",
        "<http://ex/s> <http://ex/p> \"nq\" <http://ex/g> .\n",
    );
    let mut db = Database::new("fmt");
    for f in [&nt, &ttl, &rdf] {
        update(&mut db, &format!("LOAD <{f}>"));
    }
    update(&mut db, &format!("LOAD <file://{nq}>"));
    assert_eq!(
        rows(
            &mut db,
            "SELECT ?o WHERE { <http://ex/s> <http://ex/p> ?o }"
        ),
        ["\"nt\"", "\"rdf\"", "\"ttl\""]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT ?o WHERE { GRAPH <http://ex/g> { ?s ?p ?o } }"
        ),
        ["\"nq\""]
    );

    // The same file loaded twice: its `_:b` (and `[ ]`) nodes are new nodes
    // each time, never merged with the earlier load's.
    let bn = write(
        &dir,
        "bn.ttl",
        "_:b <http://ex/name> \"b\" .\n_:b <http://ex/age> 1 .\n",
    );
    let mut db = Database::new("bn");
    update(&mut db, &format!("LOAD <{bn}>"));
    update(&mut db, &format!("LOAD <{bn}>"));
    assert_eq!(
        count(
            &mut db,
            "SELECT DISTINCT ?b WHERE { ?b <http://ex/name> \"b\" }"
        ),
        2
    );
    // Each node keeps its own two statements.
    assert_eq!(
        count(
            &mut db,
            "SELECT ?b WHERE { ?b <http://ex/name> \"b\" ; <http://ex/age> 1 }"
        ),
        2
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_load_applies_nothing_silent_or_not() {
    let dir = tmp("atomic");
    // More good statements than one import batch holds (64 Ki), so the
    // first batch is inserted before the bad statement is read.
    let mut text = String::new();
    for i in 0..(gstore::parser::import::DEFAULT_BATCH_SIZE + 1000) {
        text.push_str(&format!("<http://ex/s{i}> <http://ex/p> \"v{i}\" .\n"));
    }
    text.push_str("<http://ex/bad> <http://ex/p> \"unterminated .\n");
    let bad = write(&dir, "bad.nt", &text);
    let bad_trig = write(
        &dir,
        "bad.trig",
        "<http://ex/g> { <http://ex/a> <http://ex/p> <http://ex/b> . }\n<http://ex/x> <http://ex/p> .\n",
    );
    for into in ["", " INTO GRAPH <http://ex/t>"] {
        let mut db =
            Database::build_from_str("a", "<http://ex/k> <http://ex/p> <http://ex/k> .\n").unwrap();
        let before = dump(&mut db);
        for f in [&bad, &bad_trig] {
            let err = db.query(&format!("LOAD <{f}>{into}")).unwrap_err();
            assert!(matches!(err, GStoreError::RdfParse { .. }), "{err}");
            assert_eq!(
                dump(&mut db),
                before,
                "LOAD{into} of {f} left statements behind"
            );
            // SILENT hides the error; still nothing is applied.
            assert_eq!(update(&mut db, &format!("LOAD SILENT <{f}>{into}")), 0);
            assert_eq!(
                dump(&mut db),
                before,
                "LOAD SILENT{into} of {f} left statements behind"
            );
        }
        // In a request of several operations, the operations before the
        // failed LOAD stay applied (as after any failure); the LOAD applies
        // nothing.
        let err = db
            .query(&format!(
                "INSERT DATA {{ <http://ex/first> <http://ex/p> <http://ex/o> }} ; LOAD <{bad}>{into}"
            ))
            .unwrap_err();
        assert!(matches!(err, GStoreError::RdfParse { .. }), "{err}");
        let mut want = before.clone();
        want.push("- <http://ex/first> <http://ex/p> <http://ex/o>".into());
        want.sort();
        assert_eq!(dump(&mut db), want);
        // Inside a transaction: the failed LOAD undid itself, the rest rolls
        // back with the transaction.
        db.begin().unwrap();
        update(
            &mut db,
            "INSERT DATA { <http://ex/in_txn> <http://ex/p> <http://ex/o> }",
        );
        assert!(db.query(&format!("LOAD <{bad}>{into}")).is_err());
        assert_eq!(count(&mut db, "SELECT * WHERE { ?s ?p ?o }"), 3);
        db.rollback().unwrap();
        assert_eq!(dump(&mut db), want);
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_cancelled_load_applies_nothing_even_when_silent() {
    let dir = tmp("cancel");
    let file = write(
        &dir,
        "x.nt",
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n",
    );
    let mut db = Database::new("c");
    let cancel = Arc::new(AtomicBool::new(true));
    for q in [format!("LOAD <{file}>"), format!("LOAD SILENT <{file}>")] {
        let err = db.query_cancellable(&q, Arc::clone(&cancel)).unwrap_err();
        assert!(matches!(err, GStoreError::Cancelled), "{q}: {err}");
        assert_eq!(db.triple_num(), 0);
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn load_still_follows_the_load_policy() {
    let dir = tmp("policy");
    let inside = dir.join("inside");
    fs::create_dir_all(&inside).unwrap();
    let ok_file = write(
        &inside,
        "ok.nt",
        "<http://ex/a> <http://ex/p> <http://ex/b> .\n",
    );
    let outside = write(
        &dir,
        "out.nt",
        "<http://ex/c> <http://ex/p> <http://ex/d> .\n",
    );
    let mut db = Database::new("p");
    db.set_load_policy(LoadPolicy::restricted().with_allowed_dir(&inside));
    assert_eq!(
        update(
            &mut db,
            &format!("LOAD <{ok_file}> INTO GRAPH <http://ex/g>")
        ),
        1
    );
    for q in [
        format!("LOAD <{outside}>"),
        format!("LOAD <{outside}> INTO GRAPH <http://ex/g>"),
        "LOAD <http://example.org/data.ttl>".to_string(),
        "LOAD <http://example.org/data.ttl> INTO GRAPH <http://ex/g>".to_string(),
    ] {
        let err = db.query(&q).unwrap_err();
        assert!(
            err.to_string().contains("load policy") || err.to_string().contains("not allowed"),
            "{q}: {err}"
        );
        assert_eq!(update(&mut db, &q.replacen("LOAD", "LOAD SILENT", 1)), 0);
    }
    assert_eq!(db.triple_num(), 0);
    assert_eq!(
        count(&mut db, "SELECT * WHERE { GRAPH ?g { ?s ?p ?o } }"),
        1
    );
    let _ = fs::remove_dir_all(&dir);
}

// ---- graph management on imported graphs, every backend -------------------

/// The graph-management steps, run on `db` (holding the TriG dataset), and
/// the state they must leave.
fn graph_management(db: &mut Database) {
    let g = |db: &mut Database, graph: &str| {
        rows(
            db,
            &format!("SELECT ?s ?p ?o WHERE {{ GRAPH <http://ex/{graph}> {{ ?s ?p ?o }} }}"),
        )
        .len()
    };
    let bnode_name = |db: &mut Database, graph: &str| {
        rows(
            db,
            &format!(
                "SELECT ?n WHERE {{ GRAPH <http://ex/{graph}> {{ <http://ex/a> <http://ex/knows> ?b . \
                 ?b <http://ex/name> ?n }} }}"
            ),
        )
    };
    assert_eq!((g(db, "g1"), g(db, "g2")), (3, 1));
    // COPY a graph holding a blank node: the copy names the same node.
    update(db, "COPY <http://ex/g1> TO <http://ex/c1>");
    assert_eq!(g(db, "c1"), 3);
    assert_eq!(bnode_name(db, "c1"), ["\"x\""]);
    // ADD into an existing graph.
    update(db, "ADD <http://ex/g2> TO <http://ex/c1>");
    assert_eq!(g(db, "c1"), 4);
    // MOVE: the source graph is gone.
    update(db, "MOVE <http://ex/c1> TO <http://ex/m1>");
    assert_eq!((g(db, "c1"), g(db, "m1")), (0, 4));
    assert_eq!(bnode_name(db, "m1"), ["\"x\""]);
    // ADD the default graph to a named one, COPY a named graph to the default.
    update(db, "ADD DEFAULT TO <http://ex/m1>");
    assert_eq!(g(db, "m1"), 6);
    // WITH … DELETE/INSERT on an imported graph, through its blank node.
    update(
        db,
        "WITH <http://ex/g1> DELETE { ?b <http://ex/name> ?n } INSERT { ?b <http://ex/name> \"y\" } \
         WHERE { <http://ex/a> <http://ex/knows> ?b . ?b <http://ex/name> ?n }",
    );
    assert_eq!(bnode_name(db, "g1"), ["\"y\""]);
    assert_eq!(bnode_name(db, "m1"), ["\"x\""], "other graphs keep theirs");
    // USING: match in one graph, write to another.
    update(
        db,
        "INSERT { GRAPH <http://ex/u> { ?s <http://ex/seen> ?o } } USING <http://ex/g2> \
         WHERE { ?s <http://ex/p> ?o }",
    );
    assert_eq!(
        rows(
            db,
            "SELECT ?s ?o WHERE { GRAPH <http://ex/u> { ?s ?p ?o } }"
        ),
        ["<http://ex/c> <http://ex/d>"]
    );
    // COPY to the default graph replaces it.
    update(db, "COPY <http://ex/g2> TO DEFAULT");
    assert_eq!(
        rows(db, "SELECT ?s ?o WHERE { ?s ?p ?o }"),
        ["<http://ex/c> <http://ex/d>"]
    );
    // DROP an imported graph.
    update(db, "DROP GRAPH <http://ex/g2>");
    assert_eq!(g(db, "g2"), 0);
}

/// The state [`graph_management`] leaves, as `dump` lists it (blank nodes
/// replaced by `_`).
fn final_state(db: &mut Database) -> Vec<String> {
    let mut v: Vec<String> = dump(db)
        .into_iter()
        .map(|r| {
            r.split(' ')
                .map(|t| if t.starts_with("_:") { "_" } else { t })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    v.sort();
    v
}

fn run_on_backend(mut db: Database, reopen: Option<&Path>) {
    graph_management(&mut db);
    let state = final_state(&mut db);
    assert_eq!(state.len(), 11, "{state:#?}");
    if let Some(dir) = reopen {
        db.save(dir).unwrap();
        drop(db);
        let mut again = Database::open(dir).unwrap();
        assert_eq!(final_state(&mut again), state, "after a reopen");
        // The imported blank node still joins after the reopen.
        assert_eq!(
            rows(
                &mut again,
                "SELECT ?n WHERE { GRAPH <http://ex/m1> { <http://ex/a> <http://ex/knows> ?b . \
                 ?b <http://ex/name> ?n } }"
            ),
            ["\"x\""]
        );
    }
}

#[test]
fn graph_management_on_imported_graphs_in_memory() {
    let dir = tmp("gm_mem");
    let file = write(&dir, "data.trig", TRIG);
    let mut db = Database::new("gm");
    update(&mut db, &format!("LOAD <{file}>"));
    run_on_backend(db, None);
    // The same through a tolerant bulk build of the N-Quads file.
    let nq = write(&dir, "data.nq", NQ);
    let db = Database::build_from_files("gm2", &[&nq]).unwrap();
    let mut db = db;
    // The N-Quads file has one default-graph statement; give it the TriG's
    // second one so both runs end in the same state.
    update(
        &mut db,
        "INSERT DATA { <http://ex/d3> <http://ex/p> <http://ex/d4> }",
    );
    run_on_backend(db, None);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn graph_management_on_imported_graphs_on_disk() {
    let dir = tmp("gm_disk");
    let file = write(&dir, "data.trig", TRIG);
    let db_dir = dir.join("gm.db");
    Database::build_disk(&db_dir, &[&file]).unwrap();
    let db = Database::open(&db_dir).unwrap();
    assert_eq!(db.storage(), gstore::StorageKind::Disk);
    run_on_backend(db, Some(&db_dir));
    // A LOAD into an open disk database is written through as well.
    let mut db = Database::open(&db_dir).unwrap();
    let extra = write(
        &dir,
        "extra.trig",
        "<http://ex/g9> { _:z <http://ex/p> \"z\" . }\n",
    );
    update(&mut db, &format!("LOAD <{extra}>"));
    drop(db);
    let mut db = Database::open(&db_dir).unwrap();
    assert_eq!(
        rows(
            &mut db,
            "SELECT ?o WHERE { GRAPH <http://ex/g9> { ?s ?p ?o } }"
        ),
        ["\"z\""]
    );
    drop(db);
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(feature = "rocksdb")]
#[test]
fn graph_management_on_imported_graphs_in_rocksdb() {
    let dir = tmp("gm_rocks");
    let file = write(&dir, "data.trig", TRIG);
    let db_dir = dir.join("gm.db");
    let sources = gstore::parser::import::sources_from_paths(&[&file]).unwrap();
    let (db, _) = Database::build_rocksdb_from_sources(
        "gm",
        &db_dir,
        &sources,
        &gstore::parser::import::ImportOptions::strict(),
        &mut (),
    )
    .unwrap();
    assert_eq!(db.storage(), gstore::StorageKind::RocksDb);
    run_on_backend(db, Some(&db_dir));
    // A failed LOAD into RocksDB leaves nothing behind, after a reopen too.
    let mut db = Database::open(&db_dir).unwrap();
    let before = final_state(&mut db);
    let bad = write(
        &dir,
        "bad.trig",
        "<http://ex/g7> { <http://ex/a> <http://ex/p> <http://ex/b> . }\n<http://ex/x> <http://ex/p> .\n",
    );
    assert!(db.query(&format!("LOAD <{bad}>")).is_err());
    assert_eq!(final_state(&mut db), before);
    drop(db);
    let mut db = Database::open(&db_dir).unwrap();
    assert_eq!(final_state(&mut db), before);
    drop(db);
    let _ = fs::remove_dir_all(&dir);
}
