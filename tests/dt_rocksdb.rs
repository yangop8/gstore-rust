//! RocksDB storage-backend integration tests (feature `rocksdb` only).
//!
//! With the feature off this file compiles to nothing (the crate-level `cfg`),
//! so the default build never references the `rocksdb` crate.

#![cfg(feature = "rocksdb")]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use gstore::model::IdTriple;
use gstore::parser::sparql;
use gstore::query::{Evaluator, QueryResult};
use gstore::store::{MutableStore, StorageBackend, TripleSource, TripleStore};
use gstore::{Database, RocksStore};

/// A fixed dataset with branching, shared objects, a literal object, and a
/// predicate used by several subjects — enough to exercise every access pattern.
const DATA: &[(u32, u32, u32)] = &[
    (0, 0, 2_000_000_000), // root name "lit"      (literal object)
    (0, 1, 1),             // root contain node0
    (0, 1, 2),             // root contain node1
    (0, 1, 3),             // root contain node2
    (2, 2, 10),            // node1 own point0
    (2, 2, 11),            // node1 own point1
    (3, 2, 12),            // node2 own point2
    (1, 1, 2),             // node0 contain node1 (shared object 2)
    (1, 0, 2_000_000_001), // node0 name "lit2"
];

fn tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gstore_rocks_ut_{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

fn load<B: StorageBackend>(b: &mut B) {
    for &(s, p, o) in DATA {
        b.insert(IdTriple::new(s, p, o));
    }
}

/// A deterministic, sorted dump of *every* observable read on a backend. Two
/// backends holding the same triples must produce byte-identical fingerprints.
fn fingerprint<B: StorageBackend>(b: &B) -> String {
    let mut out = String::new();
    writeln!(out, "triple_count={}", b.triple_count()).unwrap();
    writeln!(out, "distinct_subjects={}", b.distinct_subjects()).unwrap();
    writeln!(out, "distinct_objects={}", b.distinct_objects()).unwrap();
    writeln!(out, "num_predicates={}", b.num_predicates()).unwrap();

    let mut subs = b.subject_keys();
    subs.sort_unstable();
    writeln!(out, "subject_keys={subs:?}").unwrap();
    let mut objs = b.object_keys();
    objs.sort_unstable();
    writeln!(out, "object_keys={objs:?}").unwrap();

    for id in 0..=12u32 {
        let mut v = b.po_by_s(id);
        v.sort_unstable();
        writeln!(out, "po_by_s({id})={v:?}").unwrap();
        let mut v = b.ps_by_o(id);
        v.sort_unstable();
        writeln!(out, "ps_by_o({id})={v:?}").unwrap();
    }
    for p in 0..=3u32 {
        let mut v = b.so_by_p(p);
        v.sort_unstable();
        writeln!(out, "so_by_p({p})={v:?}").unwrap();
        let mut v = b.subs_by_p(p);
        v.sort_unstable();
        writeln!(out, "subs_by_p({p})={v:?}").unwrap();
        let mut v = b.objs_by_p(p);
        v.sort_unstable();
        writeln!(out, "objs_by_p({p})={v:?}").unwrap();
        writeln!(out, "pred_card({p})={}", b.pred_card(p)).unwrap();
        writeln!(out, "pred_distinct_subj({p})={}", b.pred_distinct_subj(p)).unwrap();
        writeln!(out, "pred_distinct_obj({p})={}", b.pred_distinct_obj(p)).unwrap();
    }
    for s in 0..=4u32 {
        for p in 0..=3u32 {
            let mut v = b.o_by_sp(s, p);
            v.sort_unstable();
            writeln!(out, "o_by_sp({s},{p})={v:?}").unwrap();
        }
    }
    for p in 0..=3u32 {
        for o in 0..=12u32 {
            let mut v = b.s_by_po(p, o);
            v.sort_unstable();
            writeln!(out, "s_by_po({p},{o})={v:?}").unwrap();
        }
    }
    for s in 0..=4u32 {
        for o in 0..=12u32 {
            let mut v = b.p_by_so(s, o);
            v.sort_unstable();
            writeln!(out, "p_by_so({s},{o})={v:?}").unwrap();
            for p in 0..=3u32 {
                writeln!(out, "exists({s},{p},{o})={}", b.exists(s, p, o)).unwrap();
            }
        }
    }
    let mut all = b.iter_all();
    all.sort();
    writeln!(out, "iter_all={all:?}").unwrap();
    out
}

/// One generic function drives *both* backends; identical fingerprints prove the
/// RocksDB store answers every access pattern and statistic exactly like the
/// in-memory store — the pluggability guarantee, demonstrated for real data.
#[test]
fn rocks_matches_memory_on_all_patterns_and_stats() {
    let dir = tmp("parity");
    let mut mem = TripleStore::new();
    load(&mut mem);
    let mut rocks = RocksStore::open(&dir).unwrap();
    load(&mut rocks);

    assert_eq!(fingerprint(&mem), fingerprint(&rocks), "after bulk insert");

    // Inserting a duplicate is rejected identically.
    assert!(!mem.insert(IdTriple::new(0, 1, 1)));
    assert!(!rocks.insert(IdTriple::new(0, 1, 1)));

    // Mutate both the same way and re-compare (covers remove + counter upkeep).
    assert!(mem.remove(IdTriple::new(0, 1, 2)));
    assert!(rocks.remove(IdTriple::new(0, 1, 2)));
    assert!(!mem.remove(IdTriple::new(0, 1, 2))); // already gone
    assert!(!rocks.remove(IdTriple::new(0, 1, 2)));
    assert!(mem.insert(IdTriple::new(4, 3, 9)));
    assert!(rocks.insert(IdTriple::new(4, 3, 9)));

    assert_eq!(
        fingerprint(&mem),
        fingerprint(&rocks),
        "after remove + insert"
    );

    // Removing everything must drain both stores' counters back to zero.
    for t in TripleSource::iter_all(&mem) {
        mem.remove(t);
    }
    for t in TripleSource::iter_all(&rocks) {
        rocks.remove(t);
    }
    assert_eq!(rocks.triple_count(), 0);
    assert_eq!(rocks.distinct_subjects(), 0);
    assert_eq!(rocks.distinct_objects(), 0);
    assert_eq!(rocks.num_predicates(), 0);
    assert_eq!(fingerprint(&mem), fingerprint(&rocks), "after draining");

    cleanup(&dir);
}

/// Data and maintained statistics survive close + reopen.
#[test]
fn rocks_persists_across_reopen() {
    let dir = tmp("reopen");
    let expected;
    {
        let mut rocks = RocksStore::open(&dir).unwrap();
        load(&mut rocks);
        rocks.flush().unwrap();
        expected = fingerprint(&rocks);
        // `rocks` dropped here → the DB is closed.
    }
    let rocks = RocksStore::open(&dir).unwrap();
    assert_eq!(rocks.triple_count(), DATA.len() as u64);
    assert_eq!(fingerprint(&rocks), expected, "reopened store differs");
    cleanup(&dir);
}

fn row_count(r: QueryResult) -> usize {
    match r {
        QueryResult::Select(rs) => rs.row_count(),
        QueryResult::Ask(b) => usize::from(b),
        QueryResult::Construct(ts) => ts.len(),
        QueryResult::Update { changed } => changed,
    }
}

const SMALL: &str = "\
<root> <name> \"Bookug Lobert\" .
<root> <contain> <node0> .
<root> <contain> <node1> .
<node0> <contain> <node1> .
<node1> <own> <point0> .
<node1> <own> <point1> .
<node2> <own> <point2> .
";

/// Real SPARQL queries evaluated through the `Evaluator` against a `RocksStore`
/// return the same row counts as against the in-memory store, using one shared
/// dictionary (so ids line up). This proves the whole query stack runs unchanged
/// over the RocksDB backend.
#[test]
fn sparql_results_match_memory_backend() {
    let dir = tmp("sparql");
    let db = Database::build_from_str("mem", SMALL).unwrap();
    let mut rocks = RocksStore::open(&dir).unwrap();
    rocks.bulk_load(db.store().iter_all().collect());

    let queries = [
        "SELECT ?o WHERE { <root> <contain> ?o }",
        "SELECT ?s ?o WHERE { ?s <own> ?o }",
        "SELECT ?p ?o WHERE { <root> ?p ?o }",
        "SELECT ?n WHERE { <root> <name> ?n }",
        "SELECT ?a ?b WHERE { ?a <contain> ?mid . ?mid <own> ?b }",
        "SELECT ?s ?p ?o WHERE { ?s ?p ?o }",
        "ASK { <root> <contain> <node0> }",
        "ASK { <root> <contain> <ghost> }",
    ];
    for q in queries {
        let ast = sparql::parse(q).unwrap();
        let mem_n = row_count(
            Evaluator::new(db.dict(), db.store())
                .evaluate(&ast)
                .unwrap(),
        );
        let rocks_n = row_count(Evaluator::new(db.dict(), &rocks).evaluate(&ast).unwrap());
        assert_eq!(mem_n, rocks_n, "row-count mismatch for query: {q}");
    }
    cleanup(&dir);
}

/// The Rocks-backed `Database` constructor builds, queries, and reopens.
#[test]
fn rocksdb_database_build_query_reopen() {
    let dir = tmp("db");
    {
        let mut db = Database::build_rocksdb_from_str("rdb", &dir, SMALL).unwrap();
        assert_eq!(db.triple_num(), 7);
        let rs = db.select("SELECT ?o WHERE { <node1> <own> ?o }").unwrap();
        assert_eq!(rs.row_count(), 2);
        assert!(Database::is_rocksdb(&dir));
    }
    // Reopen from disk and query again.
    let mut db = Database::open_rocksdb(&dir).unwrap();
    assert_eq!(db.name(), "rdb");
    assert_eq!(db.triple_num(), 7);
    let rs = db
        .select("SELECT ?o WHERE { <root> <contain> ?o }")
        .unwrap();
    assert_eq!(rs.row_count(), 2);
    cleanup(&dir);
}

/// End-to-end via the *binaries*: `gbuild --rocksdb` builds a Rocks-backed DB on
/// disk, which then reopens (the `gserver --rocksdb` path) and answers SPARQL.
/// Guards the CLI wiring the HTTP / NLQA stack points at.
#[test]
fn gbuild_cli_rocksdb_builds_reopenable_db() {
    use std::process::Command;
    let base = tmp("cli");
    std::fs::create_dir_all(&base).unwrap();
    let nt = base.join("data.nt");
    std::fs::write(
        &nt,
        "<http://ex/Inception> <http://ex/directedBy> <http://ex/Nolan> .\n\
         <http://ex/Interstellar> <http://ex/directedBy> <http://ex/Nolan> .\n",
    )
    .unwrap();
    let name = base.join("movies");

    let status = Command::new(env!("CARGO_BIN_EXE_gbuild"))
        .args(["--rocksdb", name.to_str().unwrap(), nt.to_str().unwrap()])
        .status()
        .unwrap();
    assert!(status.success(), "gbuild --rocksdb exited with failure");

    let db_dir = base.join("movies.db");
    assert!(
        Database::is_rocksdb(&db_dir),
        "gbuild --rocksdb did not create a RocksDB store"
    );

    // The gserver --rocksdb path: reopen from disk and run a real query.
    let mut db = Database::open_rocksdb(&db_dir).unwrap();
    assert_eq!(db.triple_num(), 2);
    let rs = db
        .select("SELECT ?d WHERE { ?m <http://ex/directedBy> ?d }")
        .unwrap();
    assert_eq!(rs.row_count(), 2);
    cleanup(&base);
}

/// Opening a RocksDB database no longer rebuilds the VS-tree (it walked every
/// entity's edges in RocksDB): the index starts stale and queries evaluate
/// without it, with the same answers (as multisets) as the in-memory build
/// and as after an explicit `rebuild_index()`.
#[test]
fn reopen_skips_the_vstree_rebuild_with_equal_answers() {
    let dir = tmp("no_rebuild");
    let mut data = String::from(SMALL);
    for i in 0..300 {
        data.push_str(&format!(
            "<http://ex/s{i}> <http://ex/p{}> <http://ex/o{}> .\n<http://ex/s{i}> <http://ex/v> \"{}\" .\n",
            i % 7,
            i % 23,
            i % 11
        ));
    }
    drop(Database::build_rocksdb_from_str("nr", &dir, &data).unwrap());
    let mut mem = Database::build_from_str("mem", &data).unwrap();
    let mut db = Database::open_rocksdb(&dir).unwrap();
    assert!(!db.stats().index_valid, "open must not build a VS-tree");

    let queries = [
        "SELECT ?o WHERE { <root> <contain> ?o }",
        "SELECT ?s ?o WHERE { ?s <http://ex/p3> ?o . ?s <http://ex/v> \"3\" }",
        "SELECT ?s WHERE { ?s <http://ex/p1> <http://ex/o9> }",
        "SELECT ?s ?p WHERE { ?s ?p <http://ex/o4> }",
        "SELECT ?a ?b WHERE { ?a <contain> ?mid . ?mid <own> ?b }",
        "SELECT ?s ?p ?o WHERE { ?s ?p ?o }",
    ];
    let sorted = |r: QueryResult| match r {
        QueryResult::Select(rs) => {
            let mut rows = rs.rows;
            rows.sort();
            rows
        }
        other => panic!("SELECT expected, got {other:?}"),
    };
    let expected: Vec<_> = queries
        .iter()
        .map(|q| sorted(mem.query(q).unwrap()))
        .collect();
    for (q, want) in queries.iter().zip(&expected) {
        assert_eq!(&sorted(db.query(q).unwrap()), want, "without a tree: {q}");
    }
    // Inserts while the index is stale leave it stale and stay visible.
    db.query("INSERT DATA { <http://ex/s1> <http://ex/p1> <http://ex/o9> }")
        .unwrap();
    mem.query("INSERT DATA { <http://ex/s1> <http://ex/p1> <http://ex/o9> }")
        .unwrap();
    assert!(!db.stats().index_valid);
    db.rebuild_index();
    assert!(db.stats().index_valid);
    for q in queries {
        assert_eq!(
            sorted(db.query(q).unwrap()),
            sorted(mem.query(q).unwrap()),
            "with a rebuilt tree: {q}"
        );
    }
    drop(db);
    cleanup(&dir);
}
