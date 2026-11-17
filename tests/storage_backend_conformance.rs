//! One conformance suite for every storage backend (review design#17): the
//! query engine, the optimizer, the VS-tree and the analytics run on any
//! [`TripleSource`] (and write through [`MutableStore`]), so every backend
//! is held to the same contract here, by the same generic code:
//!
//! * **the model** — after the same seeded inserts and removes, every list
//!   method answers what the in-memory [`TripleStore`] answers, in the
//!   trait's ordering contract (no sorting here), and so do the global and
//!   per-predicate counts;
//! * **contract C1** — every provided count method (`count_*`,
//!   `distinct_p_of_*`, `has_*`), which a backend may override, returns
//!   exactly what its default computes from the backend's *own* lists;
//! * **writes** — `insert` / `remove` (and `insert_many` / `remove_many`)
//!   answer whether the triple was new / present, as the model's do.
//!
//! The backends: [`TripleStore`], [`Backend`] (its memory and disk variants),
//! [`DiskTriples`], [`DiskStore`] and [`DiskSource`] (the on-disk store and
//! its reporting view), [`OverlayView`] (a transaction's view over a base),
//! [`ShardedStore`] and, with the `rocksdb` feature, `RocksStore`. Before
//! this suite each had its own helper (dt_count_methods.rs, dt_rocksdb.rs,
//! dt_overlay.rs, kvstore/store.rs) and the "pluggability proof" in
//! `store/mod.rs` ran on `TripleStore` alone.
//!
//! Every temp directory is removed when its test ends (also on failure).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use gstore::cluster::ShardedStore;
use gstore::kvstore::{DiskSource, DiskStore, DiskTriples};
use gstore::model::id::{EntityLiteralId, LITERAL_FIRST_ID};
use gstore::store::read_scope::ReadScope;
use gstore::store::{Backend, Overlay, StorageBackend, TripleSource, TripleStore};
use gstore::{IdTriple, MutableStore};

// ---- the id domain and the workload -----------------------------------------

/// Subjects `0..NS`, entity objects `0..NE`, predicates `0..NP`, literal
/// objects `LITERAL_FIRST_ID + 0..NL`: small, so keys collide, empty out and
/// come back often.
const NS: u32 = 20;
const NE: u32 = 26;
const NP: u32 = 5;
const NL: u32 = 10;

/// SplitMix64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u32) -> u32 {
        (self.next() % u64::from(n)) as u32
    }

    fn triple(&mut self) -> IdTriple {
        let s = self.below(NS);
        let p = self.below(NP);
        let o = if self.below(4) == 0 {
            LITERAL_FIRST_ID + self.below(NL)
        } else {
            self.below(NE)
        };
        IdTriple::new(s, p, o)
    }
}

/// Every object id the checks probe (a few beyond the domain: never used).
fn objects() -> Vec<EntityLiteralId> {
    (0..NE + 2)
        .chain((0..NL + 2).map(|i| LITERAL_FIRST_ID + i))
        .collect()
}

/// One step of a seeded workload.
#[derive(Debug, Clone)]
enum Op {
    Insert(IdTriple),
    Remove(IdTriple),
    InsertMany(Vec<IdTriple>),
    RemoveMany(Vec<IdTriple>),
}

/// `n` seeded steps: single inserts (half), removes (a quarter), and
/// batches of each with repeats inside the batch.
fn workload(seed: u64, n: usize) -> Vec<Op> {
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    (0..n)
        .map(|_| match rng.below(8) {
            0..=3 => Op::Insert(rng.triple()),
            4 | 5 => Op::Remove(rng.triple()),
            6 => {
                let t = rng.triple();
                let k = 1 + rng.below(5) as usize;
                let mut batch: Vec<IdTriple> = (0..k).map(|_| rng.triple()).collect();
                batch.push(t);
                batch.push(t);
                Op::InsertMany(batch)
            }
            _ => Op::RemoveMany((0..1 + rng.below(5)).map(|_| rng.triple()).collect()),
        })
        .collect()
}

/// The model after `ops`.
fn model(ops: &[Op]) -> TripleStore {
    let mut m = TripleStore::new();
    for op in ops {
        apply(&mut m, op);
    }
    m
}

/// Apply `op` to `b`, returning what the writes answered.
fn apply<B: MutableStore + ?Sized>(b: &mut B, op: &Op) -> Vec<bool> {
    match op {
        Op::Insert(t) => vec![b.insert(*t)],
        Op::Remove(t) => vec![b.remove(*t)],
        Op::InsertMany(ts) => b.insert_many(ts),
        Op::RemoveMany(ts) => b.remove_many(ts),
    }
}

// ---- the contract -------------------------------------------------------------

/// The number of runs of equal first components (the default of
/// `distinct_p_of_*`).
fn runs(pairs: &[(u32, u32)]) -> u64 {
    let mut n = 0u64;
    let mut last = None;
    for &(a, _) in pairs {
        if last != Some(a) {
            n += 1;
            last = Some(a);
        }
    }
    n
}

/// Contract C1: every provided count method of `b` returns exactly what its
/// default computes from `b`'s own lists.
fn check_c1<S: TripleSource + ?Sized>(b: &S, label: &str) {
    let objs = objects();
    for p in 0..NP + 1 {
        let so = b.so_by_p(p);
        assert_eq!(
            b.count_so_by_p(p),
            so.len() as u64,
            "{label}: C1 count_so_by_p({p})"
        );
        assert_eq!(
            b.has_predicate(p),
            !so.is_empty(),
            "{label}: C1 has_predicate({p})"
        );
    }
    for s in 0..NS + 2 {
        let po = b.po_by_s(s);
        assert_eq!(
            b.count_po_by_s(s),
            po.len() as u64,
            "{label}: C1 count_po_by_s({s})"
        );
        assert_eq!(
            b.distinct_p_of_s(s),
            runs(&po),
            "{label}: C1 distinct_p_of_s({s})"
        );
        assert_eq!(
            b.has_subject(s),
            !po.is_empty(),
            "{label}: C1 has_subject({s})"
        );
        for p in 0..NP + 1 {
            assert_eq!(
                b.count_o_by_sp(s, p),
                b.o_by_sp(s, p).len() as u64,
                "{label}: C1 count_o_by_sp({s},{p})"
            );
        }
        for &o in &objs {
            assert_eq!(
                b.count_p_by_so(s, o),
                b.p_by_so(s, o).len() as u64,
                "{label}: C1 count_p_by_so({s},{o})"
            );
        }
    }
    for &o in &objs {
        let ps = b.ps_by_o(o);
        assert_eq!(
            b.count_ps_by_o(o),
            ps.len() as u64,
            "{label}: C1 count_ps_by_o({o})"
        );
        assert_eq!(
            b.distinct_p_of_o(o),
            runs(&ps),
            "{label}: C1 distinct_p_of_o({o})"
        );
        assert_eq!(
            b.has_object(o),
            !ps.is_empty(),
            "{label}: C1 has_object({o})"
        );
        for p in 0..NP + 1 {
            assert_eq!(
                b.count_s_by_po(p, o),
                b.s_by_po(p, o).len() as u64,
                "{label}: C1 count_s_by_po({p},{o})"
            );
        }
    }
}

/// Every list and count of `got` equals the model's, in the contract order
/// (through the trait: `TripleStore`'s inherent methods return slices).
fn check_model<S: TripleSource + ?Sized, W: TripleSource>(got: &S, want: &W, label: &str) {
    let objs = objects();
    assert_eq!(got.iter_all(), want.iter_all(), "{label}: iter_all");
    assert_eq!(
        got.subject_keys(),
        want.subject_keys(),
        "{label}: subject_keys"
    );
    assert_eq!(
        got.object_keys(),
        want.object_keys(),
        "{label}: object_keys"
    );
    assert_eq!(
        (
            got.triple_count(),
            got.distinct_subjects(),
            got.distinct_objects(),
            got.num_predicates()
        ),
        (
            want.triple_count(),
            want.distinct_subjects(),
            want.distinct_objects(),
            want.num_predicates()
        ),
        "{label}: triples, distinct subjects / objects, predicates"
    );
    for p in 0..NP + 1 {
        assert_eq!(got.so_by_p(p), want.so_by_p(p), "{label}: so_by_p({p})");
        assert_eq!(
            got.subs_by_p(p),
            want.subs_by_p(p),
            "{label}: subs_by_p({p})"
        );
        assert_eq!(
            got.objs_by_p(p),
            want.objs_by_p(p),
            "{label}: objs_by_p({p})"
        );
        assert_eq!(
            (
                got.pred_card(p),
                got.pred_distinct_subj(p),
                got.pred_distinct_obj(p)
            ),
            (
                want.pred_card(p),
                want.pred_distinct_subj(p),
                want.pred_distinct_obj(p)
            ),
            "{label}: predicate statistics of {p}"
        );
    }
    for s in 0..NS + 2 {
        assert_eq!(got.po_by_s(s), want.po_by_s(s), "{label}: po_by_s({s})");
        for p in 0..NP + 1 {
            assert_eq!(
                got.o_by_sp(s, p),
                want.o_by_sp(s, p),
                "{label}: o_by_sp({s},{p})"
            );
        }
        for &o in &objs {
            assert_eq!(
                got.p_by_so(s, o),
                want.p_by_so(s, o),
                "{label}: p_by_so({s},{o})"
            );
            for p in 0..NP + 1 {
                assert_eq!(
                    got.exists(s, p, o),
                    want.exists(s, p, o),
                    "{label}: exists({s},{p},{o})"
                );
            }
        }
    }
    for &o in &objs {
        assert_eq!(got.ps_by_o(o), want.ps_by_o(o), "{label}: ps_by_o({o})");
        for p in 0..NP + 1 {
            assert_eq!(
                got.s_by_po(p, o),
                want.s_by_po(p, o),
                "{label}: s_by_po({p},{o})"
            );
        }
    }
}

/// The whole read contract: the model, then C1.
fn conforms<S: TripleSource + ?Sized>(got: &S, want: &TripleStore, label: &str) {
    // A read error inside a check must fail it, not pass unnoticed.
    let scope = ReadScope::enter();
    check_model(got, want, label);
    check_c1(got, label);
    assert_eq!(scope.take_error(), None, "{label}: a read failed");
}

/// The write contract: drive `b` — holding what the model `start` holds —
/// through the seeded workload next to the model, comparing what every
/// write answers, and check the read contract along the way. Returns the
/// model of the final state.
fn exercise<B: StorageBackend + ?Sized>(
    b: &mut B,
    start: TripleStore,
    seed: u64,
    label: &str,
) -> TripleStore {
    let ops = workload(seed, 300);
    let mut m = start;
    for (i, op) in ops.iter().enumerate() {
        let want = apply(&mut m, op);
        let got = apply(b, op);
        assert_eq!(got, want, "{label}: step {i} {op:?}");
        if i % 100 == 99 {
            conforms(b, &m, &format!("{label} after step {i}"));
        }
    }
    m
}

const SEEDS: [u64; 3] = [1, 7, 42];

// ---- scratch directories ------------------------------------------------------

/// A temp directory removed on drop (also while a failing test unwinds).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "gstore_backend_conformance_{tag}_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        TmpDir(d)
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A `kvstore.kv` store at `path` whose entity ids `0..NE + 2`, predicate
/// ids `0..NP + 1` and literal ids `LITERAL_FIRST_ID + 0..NL + 2` are
/// interned (in id order) and which holds no triple, so triples can be
/// written by id.
fn seeded_disk_store(path: &Path) -> DiskStore {
    let ent = |i: u32| format!("<http://ex/e{i}>");
    let pred = |i: u32| format!("<http://ex/p{i}>");
    let lit = |i: u32| format!("\"l{i}\"");
    let mut ds = DiskStore::open(path, 16).unwrap();
    let (ents, preds) = (NE + 2, NP + 1);
    // `insert_keys` interns the subject, the predicate, then the object.
    for i in 0..ents {
        ds.insert_keys(&ent(i), &pred(i.min(preds - 1)), &ent(i), false)
            .unwrap();
    }
    for k in 0..NL + 2 {
        ds.insert_keys(&ent(0), &pred(0), &lit(k), true).unwrap();
    }
    for i in 0..ents {
        ds.delete_keys(&ent(i), &pred(i.min(preds - 1)), &ent(i), false)
            .unwrap();
    }
    for k in 0..NL + 2 {
        ds.delete_keys(&ent(0), &pred(0), &lit(k), true).unwrap();
    }
    ds.flush().unwrap();
    assert_eq!(ds.triple_count(), 0);
    ds
}

// ---- the backends --------------------------------------------------------------

#[test]
fn the_in_memory_triple_store_conforms() {
    for seed in SEEDS {
        let mut s = TripleStore::new();
        let m = exercise(
            &mut s,
            TripleStore::new(),
            seed,
            &format!("TripleStore seed {seed}"),
        );
        conforms(&s, &m, &format!("TripleStore seed {seed}"));
        // A bulk load of the same triples gives the same store.
        let mut bulk = TripleStore::new();
        MutableStore::bulk_load(&mut bulk, TripleSource::iter_all(&m));
        conforms(&bulk, &m, &format!("TripleStore bulk-loaded seed {seed}"));
    }
}

#[test]
fn the_memory_backend_conforms() {
    for seed in SEEDS {
        let mut b = Backend::Memory(TripleStore::new());
        let label = format!("Backend::Memory seed {seed}");
        let m = exercise(&mut b, TripleStore::new(), seed, &label);
        conforms(&b, &m, &label);
    }
}

#[test]
fn the_disk_backends_conform() {
    for seed in SEEDS {
        let dir = TmpDir::new(&format!("disk_{seed}"));
        let path = dir.0.join("kvstore.kv");
        let mut dt = DiskTriples::new(seeded_disk_store(&path)).unwrap();
        let label = format!("DiskTriples seed {seed}");
        let m = exercise(&mut dt, TripleStore::new(), seed, &label);
        conforms(&dt, &m, &label);
        // The committed store and its reporting view, as a Stream-mode
        // query reads them.
        let ds = dt.disk_store().unwrap();
        conforms(ds, &m, &format!("DiskStore seed {seed}"));
        conforms(&DiskSource::new(ds), &m, &format!("DiskSource seed {seed}"));
        // Reopened: the same.
        drop(dt);
        let dt = DiskTriples::open(&path, 16).unwrap();
        conforms(&dt, &m, &format!("DiskTriples reopened seed {seed}"));
        // As a database's backend (Stream mode).
        let mut b = Backend::Disk(dt);
        let more = exercise(
            &mut b,
            m,
            seed ^ 0x5A5A,
            &format!("Backend::Disk seed {seed}"),
        );
        conforms(&b, &more, &format!("Backend::Disk seed {seed}"));
    }
}

#[test]
fn a_transaction_overlay_view_conforms() {
    for seed in SEEDS {
        // A base, then a transaction's changes over it.
        let base = model(&workload(seed, 200));
        let mut overlay = Overlay::new();
        let mut m = base.clone();
        for op in workload(seed ^ 0xC0FFEE, 150) {
            let ts = match &op {
                Op::Insert(t) | Op::Remove(t) => vec![*t],
                Op::InsertMany(ts) | Op::RemoveMany(ts) => ts.clone(),
            };
            let insert = matches!(op, Op::Insert(_) | Op::InsertMany(_));
            for t in ts {
                let changed = if insert {
                    overlay.insert(&base, t)
                } else {
                    overlay.remove(&base, t)
                };
                let model_changed = if insert { m.insert(t) } else { m.remove(t) };
                assert_eq!(changed, model_changed, "OverlayView seed {seed}: {t:?}");
            }
        }
        conforms(
            &overlay.view(&base),
            &m,
            &format!("OverlayView seed {seed}"),
        );
    }
}

#[test]
fn the_sharded_store_conforms() {
    for seed in SEEDS {
        let m = model(&workload(seed, 300));
        for shards in [1, 3, 8] {
            let s = ShardedStore::from_triples(shards, m.iter_all());
            conforms(
                &s,
                &m,
                &format!("ShardedStore of {shards} shards, seed {seed}"),
            );
        }
    }
}

#[cfg(feature = "rocksdb")]
#[test]
fn the_rocksdb_store_conforms() {
    use gstore::backend::rocks::RocksStore;
    for seed in SEEDS {
        let dir = TmpDir::new(&format!("rocks_{seed}"));
        let mut r = RocksStore::open(dir.0.join("rocksdb")).unwrap();
        let label = format!("RocksStore seed {seed}");
        let m = exercise(&mut r, TripleStore::new(), seed, &label);
        conforms(&r, &m, &label);
        drop(r);
        let r = RocksStore::open(dir.0.join("rocksdb")).unwrap();
        conforms(&r, &m, &format!("RocksStore reopened seed {seed}"));
        let mut b = Backend::Rocks(r);
        let more = exercise(
            &mut b,
            m,
            seed ^ 0x5A5A,
            &format!("Backend::Rocks seed {seed}"),
        );
        conforms(&b, &more, &format!("Backend::Rocks seed {seed}"));
    }
}
