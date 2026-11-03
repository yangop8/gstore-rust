//! The copy-on-write `TripleStore` (Phase 6 R4-SNAP): clones share the
//! indexes and never see each other's later mutations, a no-op mutation
//! never copies, the serialized and `Debug` forms are byte for byte those
//! of the store before it was copy-on-write, a randomized differential
//! with periodic clones (with and without spare copies), a cross-thread
//! read of a clone while the original mutates, and `ConcurrentDb`'s
//! snapshots over the reused spare copies. Also the latency driver of the
//! package's acceptance (ignored by default).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use gstore::concurrent::ConcurrentDb;
use gstore::model::IdTriple;
use gstore::query::QueryResult;
use gstore::store::TripleStore;
use gstore::Database;
use serde::{Deserialize, Serialize};

type AdjMap = BTreeMap<u32, Vec<(u32, u32)>>;

/// The definition of `TripleStore` before it became copy-on-write (its
/// fields, order and derives, under its name): the golden reference for
/// the serialized and the `Debug` form.
mod legacy {
    use super::*;

    #[derive(Debug, Default, Clone, Serialize, Deserialize)]
    pub struct TripleStore {
        pub s2po: AdjMap,
        pub o2ps: AdjMap,
        pub p2so: AdjMap,
        pub triple_count: u64,
        #[serde(skip)]
        pub pred_stats: BTreeMap<u32, (usize, usize)>,
    }
}

/// The legacy store holding exactly `triples` (built from the set, not
/// through the store under test).
fn legacy_of(triples: &BTreeSet<IdTriple>) -> legacy::TripleStore {
    let mut l = legacy::TripleStore::default();
    for t in triples {
        l.s2po.entry(t.sub).or_default().push((t.pred, t.obj));
        l.o2ps.entry(t.obj).or_default().push((t.pred, t.sub));
        l.p2so.entry(t.pred).or_default().push((t.sub, t.obj));
    }
    for v in l
        .s2po
        .values_mut()
        .chain(l.o2ps.values_mut())
        .chain(l.p2so.values_mut())
    {
        v.sort_unstable();
    }
    l.triple_count = triples.len() as u64;
    for (&p, pairs) in &l.p2so {
        let subs: BTreeSet<u32> = pairs.iter().map(|&(s, _)| s).collect();
        let objs: BTreeSet<u32> = pairs.iter().map(|&(_, o)| o).collect();
        l.pred_stats.insert(p, (subs.len(), objs.len()));
    }
    l
}

/// `store` holds exactly `expected`: the same serialized bytes and `Debug`
/// text as the legacy store of that set (so the derived statistics too),
/// and the same triples.
fn assert_holds(store: &TripleStore, expected: &BTreeSet<IdTriple>, what: &str) {
    let all: BTreeSet<IdTriple> = store.iter_all().collect();
    assert_eq!(&all, expected, "{what}: triples");
    assert_eq!(store.triple_count(), expected.len() as u64, "{what}: count");
    let legacy = legacy_of(expected);
    assert_eq!(
        bincode::serialize(store).unwrap(),
        bincode::serialize(&legacy).unwrap(),
        "{what}: serialized form"
    );
    assert_eq!(
        format!("{store:?}"),
        format!("{legacy:?}"),
        "{what}: Debug form"
    );
}

/// A small deterministic generator (no extra crate).
struct Rng(u64);

impl Rng {
    fn next(&mut self, n: u32) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) % u64::from(n)) as u32
    }
    fn triple(&mut self) -> IdTriple {
        IdTriple::new(self.next(40), self.next(6), self.next(60))
    }
}

fn set_of(triples: &[IdTriple]) -> BTreeSet<IdTriple> {
    triples.iter().copied().collect()
}

#[test]
fn clones_share_until_one_side_mutates_and_never_see_each_other() {
    let mut rng = Rng(7);
    let triples: Vec<IdTriple> = (0..300).map(|_| rng.triple()).collect();
    let mut a = TripleStore::new();
    a.bulk_load(triples.clone());
    let before = set_of(&triples);
    let b = a.clone();
    assert!(a.shares_storage_with(&b), "a clone shares the indexes");

    // The original mutates: the clone keeps the state it was taken at.
    let fresh = IdTriple::new(1000, 1, 1001);
    assert!(a.insert(fresh));
    assert!(!a.shares_storage_with(&b), "the first write copies");
    let gone = *before.iter().next().unwrap();
    assert!(a.remove(gone));
    let mut after = before.clone();
    after.insert(fresh);
    after.remove(&gone);
    assert_holds(&a, &after, "original after its writes");
    assert_holds(&b, &before, "clone after the original's writes");

    // The clone mutates: the original is untouched.
    let mut c = a.clone();
    let other = IdTriple::new(2000, 2, 2001);
    assert!(c.insert(other));
    assert!(c.remove(fresh));
    assert_holds(&a, &after, "original after the clone's writes");
    let mut c_expected = after.clone();
    c_expected.insert(other);
    c_expected.remove(&fresh);
    assert_holds(&c, &c_expected, "clone after its writes");

    // A store that is no longer shared mutates in place.
    drop(b);
    let d = a.clone();
    drop(d);
    let probe = a.clone();
    assert!(a.shares_storage_with(&probe));
    drop(probe);
    assert!(a.insert(IdTriple::new(3000, 3, 3001)));
}

#[test]
fn a_mutation_that_changes_nothing_never_copies() {
    let mut rng = Rng(11);
    let triples: Vec<IdTriple> = (0..200).map(|_| rng.triple()).collect();
    let mut a = TripleStore::new();
    a.bulk_load(triples.clone());
    let b = a.clone();
    let present = triples[3];
    let absent = IdTriple::new(999, 9, 999);
    assert!(!a.insert(present), "a present triple is not inserted");
    assert!(!a.remove(absent), "an absent triple is not removed");
    a.bulk_load(Vec::new());
    a.bulk_load(triples[..50].to_vec());
    assert!(
        a.shares_storage_with(&b),
        "no-op insert / remove / bulk loads must not copy shared indexes"
    );
    // With a spare kept as well.
    a.keep_spare_copies();
    assert!(!a.insert(present));
    assert!(!a.remove(absent));
    assert!(a.shares_storage_with(&b));
    assert!(!a.has_spare_copy());
    // A real change copies once.
    assert!(a.insert(absent));
    assert!(!a.shares_storage_with(&b));
    assert!(a.has_spare_copy(), "the shared indexes became the spare");
    assert_holds(&b, &set_of(&triples), "the clone");
}

/// The serialized form is the legacy one byte for byte (and reads back to
/// the same store, statistics included), for random stores and for a
/// fixed golden byte string.
#[test]
fn serialized_and_debug_forms_equal_the_legacy_store() {
    let mut rng = Rng(23);
    for round in 0..20 {
        let n = (round * 37) % 400;
        let triples: Vec<IdTriple> = (0..n).map(|_| rng.triple()).collect();
        let set = set_of(&triples);
        let mut store = TripleStore::new();
        store.bulk_load(triples.clone());
        assert_holds(&store, &set, "bulk-loaded");
        // Read back from the legacy bytes: the same store, statistics too.
        let bytes = bincode::serialize(&legacy_of(&set)).unwrap();
        let back: TripleStore = bincode::deserialize(&bytes).unwrap();
        assert_holds(&back, &set, "read from legacy bytes");
        let legacy_back: legacy::TripleStore =
            bincode::deserialize(&bincode::serialize(&store).unwrap()).unwrap();
        assert_eq!(legacy_back.triple_count, set.len() as u64);
    }
    // Golden bytes of a two-triple store: (1,2,3) and (1,2,4).
    let mut s = TripleStore::new();
    s.insert(IdTriple::new(1, 2, 3));
    s.insert(IdTriple::new(1, 2, 4));
    let golden: Vec<u8> = vec![
        // s2po: 1 entry, key 1, 2 pairs (2,3) (2,4)
        1, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0, 2, 0, 0,
        0, 4, 0, 0, 0, // o2ps: keys 3 and 4, one pair (2,1) each
        2, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0, 4, 0,
        0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0,
        // p2so: key 2, pairs (1,3) (1,4)
        1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 3, 0, 0, 0, 1, 0, 0,
        0, 4, 0, 0, 0, // triple_count
        2, 0, 0, 0, 0, 0, 0, 0,
    ];
    assert_eq!(bincode::serialize(&s).unwrap(), golden);
    assert_eq!(
        format!("{s:?}"),
        "TripleStore { s2po: {1: [(2, 3), (2, 4)]}, o2ps: {3: [(2, 1)], 4: [(2, 1)]}, \
         p2so: {2: [(1, 3), (1, 4)]}, triple_count: 2, pred_stats: {2: (1, 2)} }"
    );
}

/// Random inserts, removes and bulk loads against a set model, with clones
/// taken (published), held and dropped at random — once without and once
/// with spare copies. After every step the store holds the model's
/// triples, and every clone still holds what the store held when it was
/// taken.
#[test]
fn randomized_differential_with_periodic_clones() {
    for (seed, spares) in [(1u64, false), (2, true), (3, true), (4, false), (5, true)] {
        let mut rng = Rng(seed);
        let mut store = TripleStore::new();
        if spares {
            store.keep_spare_copies();
        }
        let mut model: BTreeSet<IdTriple> = BTreeSet::new();
        // Clones still held, with the state they were taken at.
        let mut held: Vec<(TripleStore, BTreeSet<IdTriple>)> = Vec::new();
        let mut published: Option<(TripleStore, BTreeSet<IdTriple>)> = None;
        let mut reused = 0usize;
        for step in 0..3000 {
            match rng.next(100) {
                0..=44 => {
                    let t = rng.triple();
                    assert_eq!(store.insert(t), model.insert(t), "insert at {step}");
                }
                45..=79 => {
                    // Mostly a present triple, so removes do happen.
                    let t = if !model.is_empty() && rng.next(4) != 0 {
                        *model
                            .iter()
                            .nth(rng.next(model.len() as u32) as usize)
                            .unwrap()
                    } else {
                        rng.triple()
                    };
                    assert_eq!(store.remove(t), model.remove(&t), "remove at {step}");
                }
                80..=82 => {
                    let batch: Vec<IdTriple> = (0..rng.next(30)).map(|_| rng.triple()).collect();
                    model.extend(batch.iter().copied());
                    store.bulk_load(batch);
                }
                83..=94 => {
                    // Publish: the previous published clone is let go.
                    let with_spare = store.has_spare_copy();
                    published = Some((store.clone(), model.clone()));
                    if with_spare {
                        reused += 1;
                    }
                }
                95..=97 => held.push((store.clone(), model.clone())),
                _ => {
                    if !held.is_empty() {
                        let i = rng.next(held.len() as u32) as usize;
                        let (clone, expected) = held.swap_remove(i);
                        assert_holds(&clone, &expected, "a held clone when dropped");
                    }
                }
            }
            if step % 97 == 0 {
                assert_holds(&store, &model, "the store");
                for (clone, expected) in &held {
                    assert_holds(clone, expected, "a held clone");
                }
                if let Some((clone, expected)) = &published {
                    assert_holds(clone, expected, "the published clone");
                }
            }
        }
        assert_holds(&store, &model, "the store at the end");
        for (clone, expected) in &held {
            assert_holds(clone, expected, "a held clone at the end");
        }
        if spares {
            assert!(reused > 0, "seed {seed}: a spare copy was in use");
        } else {
            assert!(!store.has_spare_copy());
        }
    }
}

/// A clone read on another thread while the original mutates: the reader
/// always sees exactly the state it was cloned at.
#[test]
fn a_clone_reads_on_another_thread_while_the_original_mutates() {
    let mut rng = Rng(99);
    let triples: Vec<IdTriple> = (0..2000).map(|_| rng.triple()).collect();
    let mut store = TripleStore::new();
    store.keep_spare_copies();
    store.bulk_load(triples.clone());
    let expected = set_of(&triples);
    let clone = store.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let stop = Arc::clone(&stop);
        let expected = expected.clone();
        std::thread::spawn(move || {
            let mut reads = 0u64;
            while !stop.load(Ordering::SeqCst) || reads < 20 {
                let all: BTreeSet<IdTriple> = clone.iter_all().collect();
                assert_eq!(all, expected, "the clone changed under its reader");
                for t in expected.iter().take(50) {
                    assert!(clone.exists(t.sub, t.pred, t.obj));
                    assert!(clone.po_by_s(t.sub).contains(&(t.pred, t.obj)));
                    assert!(clone.ps_by_o(t.obj).contains(&(t.pred, t.sub)));
                }
                reads += 1;
            }
            reads
        })
    };
    let mut model = expected.clone();
    for i in 0..20_000u32 {
        let t = rng.triple();
        if i % 3 == 0 {
            store.remove(t);
            model.remove(&t);
        } else {
            store.insert(t);
            model.insert(t);
        }
        if i % 1000 == 0 {
            // Publishing (and dropping) more clones meanwhile.
            drop(store.clone());
        }
    }
    stop.store(true, Ordering::SeqCst);
    let reads = reader.join().expect("the reader saw a changed clone");
    assert!(reads >= 20);
    assert_holds(&store, &model, "the original");
}

fn count(result: QueryResult) -> usize {
    match result {
        QueryResult::Select(rs) => rs.rows.len(),
        other => panic!("not a SELECT: {other:?}"),
    }
}

/// `ConcurrentDb` publishes share the store; its spare copies keep every
/// snapshot exact while the writer keeps updating, readers holding old
/// snapshots included.
#[test]
fn concurrent_db_snapshots_stay_exact_over_reused_spares() {
    let mut doc = String::new();
    for i in 0..200 {
        doc.push_str(&format!(
            "<http://e/s{i}> <http://e/p> <http://e/o{}> .\n",
            i % 7
        ));
    }
    let cdb = Arc::new(ConcurrentDb::build_from_str("cow", &doc).unwrap());
    let all = "SELECT ?s ?o WHERE { ?s <http://e/p> ?o }";
    assert_eq!(count(cdb.query(all).unwrap()), 200);
    let mut held = Vec::new();
    let mut expected = 200usize;
    for i in 0..60 {
        let t = format!(
            "<http://e/s{}> <http://e/p> <http://e/o{}>",
            i % 13,
            100 + i
        );
        assert_eq!(cdb.update(&format!("INSERT DATA {{ {t} }}")).unwrap(), 1);
        expected += 1;
        if i % 3 == 0 {
            assert_eq!(cdb.update(&format!("DELETE DATA {{ {t} }}")).unwrap(), 1);
            expected -= 1;
        }
        if i % 7 == 0 {
            held.push((cdb.snapshot(), expected));
        }
        if i % 11 == 0 && !held.is_empty() {
            held.remove(0);
        }
        assert_eq!(count(cdb.query(all).unwrap()), expected, "update {i}");
        for (snap, n) in &held {
            assert_eq!(count(snap.query(all).unwrap()), *n, "held snapshot");
        }
    }
    // Readers on other threads while the writer updates.
    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..3)
        .map(|_| {
            let cdb = Arc::clone(&cdb);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    let snap = cdb.snapshot();
                    let n = count(snap.query(all).unwrap());
                    // Pairs are inserted and deleted whole, one update each:
                    // a snapshot sees the base count or one more.
                    assert!(n == expected || n == expected + 1, "torn read: {n}");
                    assert_eq!(count(snap.query(all).unwrap()), n, "snapshot moved");
                }
            })
        })
        .collect();
    for i in 0..200 {
        let t = format!("<http://e/x{i}> <http://e/p> <http://e/y{i}>");
        cdb.update(&format!("INSERT DATA {{ {t} }}")).unwrap();
        cdb.update(&format!("DELETE DATA {{ {t} }}")).unwrap();
    }
    stop.store(true, Ordering::SeqCst);
    for r in readers {
        r.join().expect("a reader failed");
    }
    assert_eq!(count(cdb.query(all).unwrap()), expected);
}

/// Warm latency of the LUBM queries on an opened database (an A/B driver
/// for the copy-on-write store's read path; run with
/// `GSTORE_COW_BENCH_DB=<db dir> cargo test --release --test dt_cow_store
/// -- --ignored --nocapture memory_query_latency`). Prints each query's
/// median of 60 runs (after 5 warm-up runs) in microseconds.
#[test]
#[ignore = "a timing driver: needs GSTORE_COW_BENCH_DB"]
fn memory_query_latency() {
    let Ok(dir) = std::env::var("GSTORE_COW_BENCH_DB") else {
        eprintln!("GSTORE_COW_BENCH_DB not set: skipped");
        return;
    };
    let mut db = Database::open(&dir).expect("open");
    db.set_query_cache_size(0);
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/lubm");
    let mut line = String::new();
    let only: Option<u32> = std::env::var("GSTORE_COW_BENCH_Q")
        .ok()
        .and_then(|q| q.parse().ok());
    for q in 1..=14 {
        if only.is_some_and(|o| o != q) {
            continue;
        }
        let text = std::fs::read_to_string(root.join(format!("lubm_q{q}.rq"))).unwrap();
        for _ in 0..5 {
            db.query(&text).unwrap();
        }
        let mut samples: Vec<f64> = (0..60)
            .map(|_| {
                let t0 = Instant::now();
                db.query(&text).unwrap();
                t0.elapsed().as_secs_f64() * 1e6
            })
            .collect();
        samples.sort_by(f64::total_cmp);
        line.push_str(&format!(" q{q}={:.1}", samples[samples.len() / 2]));
    }
    println!("memory_query_latency (us):{line}");
}

/// Latency of a `ConcurrentDb` one-triple update whose terms all exist
/// (the R4-SNAP acceptance driver; run with
/// `GSTORE_COW_BENCH_NT=<file.nt> cargo test --release --test dt_cow_store
/// -- --ignored --nocapture concurrent_update_latency`). Prints the median,
/// mean and 90th percentile in milliseconds — with no reader, and with a
/// reader holding the snapshots of the last two versions (the older one
/// shares the spare copy, which then cannot be reused: every write copies
/// the store, the fallback).
#[test]
#[ignore = "a timing driver: needs GSTORE_COW_BENCH_NT"]
fn concurrent_update_latency() {
    let Ok(path) = std::env::var("GSTORE_COW_BENCH_NT") else {
        eprintln!("GSTORE_COW_BENCH_NT not set: skipped");
        return;
    };
    let rounds: usize = std::env::var("GSTORE_COW_BENCH_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(40);
    let started = Instant::now();
    let db = Database::build_from_files("cow-bench", &[&path]).expect("build");
    eprintln!(
        "built {} triples in {:.1} s",
        db.triple_num(),
        started.elapsed().as_secs_f64()
    );
    let cdb = ConcurrentDb::new(db);
    // Every term exists in LUBM-like data; the triple itself does not.
    let t = "<http://www.University0.edu> \
             <http://www.lehigh.edu/~zhp2/2004/0401/univ-bench.owl#name> \
             <http://www.Department0.University0.edu>";
    let insert = format!("INSERT DATA {{ {t} }}");
    let delete = format!("DELETE DATA {{ {t} }}");
    for hold in [false, true] {
        let mut samples = Vec::with_capacity(rounds * 2);
        // The snapshots of the last two versions: the older one shares the
        // spare copy, so the next update cannot reuse it.
        let mut previous = std::collections::VecDeque::new();
        for i in 0..rounds + 2 {
            for q in [&insert, &delete] {
                if hold {
                    previous.push_back(cdb.snapshot());
                    if previous.len() > 2 {
                        previous.pop_front();
                    }
                }
                let t0 = Instant::now();
                let changed = cdb.update(q).expect("update");
                let ms = t0.elapsed().as_secs_f64() * 1e3;
                assert_eq!(changed, 1);
                if i >= 2 {
                    samples.push(ms);
                }
            }
        }
        drop(previous);
        samples.sort_by(f64::total_cmp);
        let mean = samples.iter().sum::<f64>() / samples.len() as f64;
        let p = |q: f64| samples[((samples.len() - 1) as f64 * q).round() as usize];
        println!(
            "concurrent_update_latency (a reader holds the last two snapshots: {hold}): n={} \
             median={:.3} ms mean={:.3} ms p90={:.3} ms min={:.3} ms",
            samples.len(),
            p(0.5),
            mean,
            p(0.9),
            samples[0]
        );
    }
}
