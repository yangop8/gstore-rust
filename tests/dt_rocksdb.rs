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

/// The temp directories the current test thread made, removed when the
/// thread ends — also when the test fails (a thread-local's destructor runs
/// after the unwinding).
struct TmpDirs(std::cell::RefCell<Vec<std::path::PathBuf>>);

impl Drop for TmpDirs {
    fn drop(&mut self) {
        for p in self.0.borrow().iter() {
            // A directory, or a file (a store path).
            let _ = std::fs::remove_dir_all(p).or_else(|_| std::fs::remove_file(p));
        }
    }
}

thread_local! {
    static TMP_DIRS: TmpDirs = const { TmpDirs(std::cell::RefCell::new(Vec::new())) };
}

fn tmp(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gstore_rocks_ut_{tag}"));
    TMP_DIRS.with(|t| t.0.borrow_mut().push(dir.clone()));
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
    // ... and every list comes in the in-memory store's (the contract) order.
    assert_eq!(
        fingerprint_in_order(&mem, 13, 4),
        fingerprint_in_order(&rocks, 13, 4),
        "in order"
    );

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
    assert_eq!(
        fingerprint_in_order(&mem, 13, 4),
        fingerprint_in_order(&rocks, 13, 4),
        "in order, after remove + insert"
    );
    rocks.check_stats().unwrap();

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

// ---- Phase 6 R3: contract order, exact counts, the counter mirror ----------

/// Every list read of `b` **in the order returned** — no sorting here, so a
/// backend that returns the right elements in another order differs — and
/// every count next to the list it counts.
fn fingerprint_in_order<B: TripleSource>(b: &B, ids: u32, preds: u32) -> String {
    let mut out = String::new();
    writeln!(out, "iter_all={:?}", b.iter_all()).unwrap();
    writeln!(out, "subject_keys={:?}", b.subject_keys()).unwrap();
    writeln!(out, "object_keys={:?}", b.object_keys()).unwrap();
    writeln!(
        out,
        "globals={} {} {} {}",
        b.triple_count(),
        b.distinct_subjects(),
        b.distinct_objects(),
        b.num_predicates()
    )
    .unwrap();
    let all_ids = (0..ids).chain([2_000_000_000, 2_000_000_001, 2_000_000_002]);
    for p in 0..preds {
        writeln!(
            out,
            "p{p}: {:?} {:?} {:?} {} {} {} {} {}",
            b.so_by_p(p),
            b.subs_by_p(p),
            b.objs_by_p(p),
            b.pred_card(p),
            b.pred_distinct_subj(p),
            b.pred_distinct_obj(p),
            b.count_so_by_p(p),
            b.has_predicate(p)
        )
        .unwrap();
    }
    for id in all_ids.clone() {
        writeln!(
            out,
            "{id}: {:?} {} {} {} | {:?} {} {} {}",
            b.po_by_s(id),
            b.count_po_by_s(id),
            b.distinct_p_of_s(id),
            b.has_subject(id),
            b.ps_by_o(id),
            b.count_ps_by_o(id),
            b.distinct_p_of_o(id),
            b.has_object(id)
        )
        .unwrap();
        for p in 0..preds {
            writeln!(
                out,
                "  {id},{p}: {:?} {} {:?} {}",
                b.o_by_sp(id, p),
                b.count_o_by_sp(id, p),
                b.s_by_po(p, id),
                b.count_s_by_po(p, id)
            )
            .unwrap();
        }
        for o in all_ids.clone().take(8) {
            writeln!(
                out,
                "  {id}-{o}: {:?} {}",
                b.p_by_so(id, o),
                b.count_p_by_so(id, o)
            )
            .unwrap();
        }
    }
    out
}

/// What the contract-C1 defaults return, computed from `b`'s own lists:
/// every override must equal it exactly.
fn assert_counts_equal_the_defaults<B: TripleSource>(b: &B, ids: u32, preds: u32, what: &str) {
    let runs = |v: &[(u32, u32)]| {
        let mut n = 0u64;
        let mut last = None;
        for &(a, _) in v {
            if last != Some(a) {
                n += 1;
                last = Some(a);
            }
        }
        n
    };
    for p in 0..preds {
        let so = b.so_by_p(p);
        assert_eq!(
            b.count_so_by_p(p),
            so.len() as u64,
            "{what}: count_so_by_p({p})"
        );
        assert_eq!(
            b.has_predicate(p),
            !so.is_empty(),
            "{what}: has_predicate({p})"
        );
    }
    for id in (0..ids).chain([2_000_000_000, 2_000_000_001]) {
        let po = b.po_by_s(id);
        let ps = b.ps_by_o(id);
        assert_eq!(
            b.count_po_by_s(id),
            po.len() as u64,
            "{what}: count_po_by_s({id})"
        );
        assert_eq!(
            b.count_ps_by_o(id),
            ps.len() as u64,
            "{what}: count_ps_by_o({id})"
        );
        assert_eq!(
            b.distinct_p_of_s(id),
            runs(&po),
            "{what}: distinct_p_of_s({id})"
        );
        assert_eq!(
            b.distinct_p_of_o(id),
            runs(&ps),
            "{what}: distinct_p_of_o({id})"
        );
        assert_eq!(
            b.has_subject(id),
            !po.is_empty(),
            "{what}: has_subject({id})"
        );
        assert_eq!(b.has_object(id), !ps.is_empty(), "{what}: has_object({id})");
        for p in 0..preds {
            assert_eq!(
                b.count_o_by_sp(id, p),
                b.o_by_sp(id, p).len() as u64,
                "{what}: count_o_by_sp({id},{p})"
            );
            assert_eq!(
                b.count_s_by_po(p, id),
                b.s_by_po(p, id).len() as u64,
                "{what}: count_s_by_po({p},{id})"
            );
        }
        for o in 0..ids.min(12) {
            assert_eq!(
                b.count_p_by_so(id, o),
                b.p_by_so(id, o).len() as u64,
                "{what}: count_p_by_so({id},{o})"
            );
        }
    }
}

/// A small deterministic generator (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u32) -> u32 {
        (self.next() % u64::from(n)) as u32
    }
    /// A random triple over `ids` entities, `preds` predicates and a few
    /// literal objects; subject 3 has many objects under predicate 1 (the
    /// long runs `distinct_p_of_s` seeks past) and object 7 many subjects.
    fn triple(&mut self, ids: u32, preds: u32) -> IdTriple {
        if self.below(6) == 0 {
            return IdTriple::new(3, 1, self.below(ids));
        }
        let s = self.below(ids);
        let p = self.below(preds);
        let o = match self.below(10) {
            0..=1 => 2_000_000_000 + self.below(3),
            2 => 7,
            _ => self.below(ids),
        };
        IdTriple::new(s, p, o)
    }
}

/// The overrides return exactly what the defaults return (randomized, with
/// inserts and removes, several seeds), every list comes in the contract
/// order of the in-memory store, and the counters stay exact throughout.
#[test]
fn counts_and_order_equal_the_defaults_and_the_memory_store() {
    const IDS: u32 = 40;
    const PREDS: u32 = 5;
    for seed in [1u64, 7, 42] {
        let dir = tmp(&format!("c1eq_{seed}"));
        let mut rocks = RocksStore::open(&dir).unwrap();
        let mut mem = TripleStore::new();
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        for step in 0..1500 {
            let t = rng.triple(IDS, PREDS);
            if rng.below(3) == 0 {
                assert_eq!(rocks.remove(t), mem.remove(t), "remove {t:?}");
            } else {
                assert_eq!(rocks.insert(t), mem.insert(t), "insert {t:?}");
            }
            if step % 500 == 499 {
                let what = format!("seed {seed} step {step}");
                rocks
                    .check_stats()
                    .unwrap_or_else(|e| panic!("{what}: {e}"));
                assert_counts_equal_the_defaults(&rocks, IDS, PREDS, &what);
                assert_eq!(
                    fingerprint_in_order(&rocks, IDS, PREDS),
                    fingerprint_in_order(&mem, IDS, PREDS),
                    "{what}: reads differ from the in-memory store"
                );
            }
        }
        drop(rocks);
        cleanup(&dir);
    }
}

/// `so_by_p` answers a predicate with long object runs per subject (SPO
/// seeks over its `x|p` subjects) and one with short runs (its POS range,
/// sorted) in the same contract order as the in-memory store.
#[test]
fn so_by_p_orders_long_and_short_runs_alike() {
    let dir = tmp("so_by_p");
    let mut rocks = RocksStore::open(&dir).unwrap();
    let mut mem = TripleStore::new();
    let mut triples = Vec::new();
    // Predicate 0: 3 subjects × 100 objects (long runs).
    for s in [9u32, 2, 5] {
        for i in 0..100u32 {
            triples.push(IdTriple::new(s, 0, (i * 37) % 101 + 1000));
        }
    }
    // Predicate 1: 300 subjects with 1–2 objects each (short runs), objects
    // falling as subjects rise; predicate 2: one object for every subject.
    for s in 0..300u32 {
        triples.push(IdTriple::new(s, 1, 5000 - s));
        if s % 3 == 0 {
            triples.push(IdTriple::new(s, 1, 2_000_000_000 + s % 7));
        }
        triples.push(IdTriple::new(s, 2, 77));
    }
    for t in triples.iter().rev() {
        assert_eq!(rocks.insert(*t), mem.insert(*t));
    }
    for p in 0..4u32 {
        assert_eq!(rocks.so_by_p(p), mem.so_by_p(p).to_vec(), "so_by_p({p})");
        assert_eq!(
            TripleSource::subs_by_p(&rocks, p),
            mem.subs_by_p(p),
            "subs_by_p({p})"
        );
        assert_eq!(TripleSource::objs_by_p(&rocks, p), mem.objs_by_p(p));
    }
    assert_eq!(
        TripleSource::iter_all(&rocks),
        TripleSource::iter_all(&mem),
        "iter_all in (p, s, o) order"
    );
    rocks.check_stats().unwrap();
    drop(rocks);
    cleanup(&dir);
}

/// Apply `n` random inserts / removes to `rocks` (and to `mem`, checking
/// that both report the same changes).
fn churn(
    rng: &mut Rng,
    rocks: &mut RocksStore,
    mut mem: Option<&mut TripleStore>,
    n: usize,
    ids: u32,
    preds: u32,
) {
    for _ in 0..n {
        let t = rng.triple(ids, preds);
        let insert = rng.below(3) != 0;
        let changed = if insert {
            rocks.insert(t)
        } else {
            rocks.remove(t)
        };
        if let Some(m) = mem.as_deref_mut() {
            let expect = if insert { m.insert(t) } else { m.remove(t) };
            assert_eq!(changed, expect, "{t:?}");
        }
    }
}

/// The in-memory `c|`/`u|`/`v|` mirror (and the cached globals) equal the
/// counters stored in RocksDB across inserts, deletes, a committed and a
/// rolled-back transaction, a crash with a transaction open, and reopens.
#[test]
fn the_counter_mirror_equals_the_stored_counters() {
    const IDS: u32 = 30;
    const PREDS: u32 = 4;
    let dir = tmp("mirror");
    let mut rng = Rng(0x5eed);
    let mut rocks = RocksStore::open(&dir).unwrap();
    let mut mem = TripleStore::new();
    churn(&mut rng, &mut rocks, Some(&mut mem), 400, IDS, PREDS);
    rocks.check_stats().unwrap();
    let before = fingerprint_in_order(&rocks, IDS, PREDS);
    assert_eq!(before, fingerprint_in_order(&mem, IDS, PREDS));

    // Rolled back: everything as before, the mirror reloaded.
    rocks.begin_journal().unwrap();
    churn(&mut rng, &mut rocks, None, 200, IDS, PREDS);
    rocks.check_stats().unwrap();
    rocks.rollback_journal().unwrap();
    rocks.check_stats().unwrap();
    assert_eq!(fingerprint_in_order(&rocks, IDS, PREDS), before);

    // Committed.
    rocks.begin_journal().unwrap();
    churn(&mut rng, &mut rocks, Some(&mut mem), 200, IDS, PREDS);
    rocks.commit_journal().unwrap();
    rocks.check_stats().unwrap();
    let committed = fingerprint_in_order(&rocks, IDS, PREDS);
    assert_eq!(committed, fingerprint_in_order(&mem, IDS, PREDS));

    // A crash with a transaction open: the reopen undoes it.
    rocks.begin_journal().unwrap();
    churn(&mut rng, &mut rocks, None, 150, IDS, PREDS);
    drop(rocks);
    let mut rocks = RocksStore::open(&dir).unwrap();
    rocks.check_stats().unwrap();
    assert_eq!(fingerprint_in_order(&rocks, IDS, PREDS), committed);

    // Drain every triple: all counters gone, globals zero.
    for t in TripleSource::iter_all(&rocks) {
        assert!(rocks.remove(t));
    }
    rocks.check_stats().unwrap();
    assert_eq!(rocks.predicates(), Vec::<u32>::new());
    drop(rocks);
    let rocks = RocksStore::open(&dir).unwrap();
    rocks.check_stats().unwrap();
    assert_eq!(rocks.triple_count(), 0);
    drop(rocks);
    cleanup(&dir);
}

/// A read error (injected) makes the statement fail instead of answering
/// from a shortened list, and a write that cannot read changes nothing;
/// once reads work again the database answers as before.
#[test]
fn an_injected_read_error_fails_the_query() {
    let dir = tmp("read_error");
    let mut db = Database::build_rocksdb_from_str("re", &dir, SMALL).unwrap();
    let q = "SELECT ?o WHERE { <root> <contain> ?o }";
    // (Not run before the failure: a cached answer would be served without
    // reading.)
    assert_eq!(db.triple_num(), 7);
    db.backend().as_rocks().unwrap().debug_fail_reads(true);
    for q in [
        q,
        "SELECT ?s ?o WHERE { ?s <own> ?o }",
        "SELECT ?s ?p ?o WHERE { ?s ?p ?o }",
        "ASK { <root> <contain> <node0> }",
        "SELECT ?a ?b WHERE { ?a <contain> ?mid . ?mid <own> ?b }",
    ] {
        let r = db.query(q);
        assert!(r.is_err(), "{q} answered {r:?} despite a read error");
    }
    let r = db.query("INSERT DATA { <root> <contain> <node9> }");
    assert!(r.is_err(), "an insert that cannot read answered {r:?}");
    db.backend().as_rocks().unwrap().debug_fail_reads(false);
    assert_eq!(db.select(q).unwrap().row_count(), 2);
    assert_eq!(db.triple_num(), 7);
    db.backend().as_rocks().unwrap().check_stats().unwrap();
    drop(db);
    cleanup(&dir);
}
