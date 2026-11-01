//! R2-DISKLIB (contract C5): the exact transaction overlay and `DiskTriples`.
//!
//! * A seeded model test drives random inserts and removes through an
//!   [`Overlay`] (over an in-memory base) and through a `DiskTriples`
//!   transaction (over a disk base) and compares **every** `TripleSource`
//!   method — lists in order, counts, distinct counts, `has_*` — and every
//!   "changed" flag with a materialized `TripleStore`.
//! * `DiskTriples`: a transaction never touches the pager before commit;
//!   commit applies, rollback does not; the broken state; drop flushes; views
//!   expire after a write and are closed after drop; read errors reach the
//!   statement's scope and only that thread's scope.
//! * LUBM Q1–Q14 evaluate identically (rows in order, same plans) over
//!   `DiskTriples` — with and without an open transaction — and over a
//!   materialized store holding the same triples.
//! * Snapshot readers running long "queries" (they loop until cancelled)
//!   while a committer commits: each commit finishes within 200 ms of
//!   cancelling them, and a reader only ever sees a committed state.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex, RwLock};
use std::time::{Duration, Instant};

use gstore::kvstore::{DiskReadView, DiskSource, DiskStore, DiskTriples, ViewError};
use gstore::model::id::{EntityLiteralId, LITERAL_FIRST_ID};
use gstore::query::{Evaluator, QueryResult};
use gstore::store::read_scope::ReadScope;
use gstore::store::{Overlay, TripleSource, TripleStore};
use gstore::{IdTriple, MutableStore};

// ---- helpers -----------------------------------------------------------------

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
}

/// The id domain of the model tests: small, so keys collide, empty out and
/// come back often.
const NS: u32 = 20; // subjects 0..NS (entities)
const NE: u32 = 26; // entity objects 0..NE
const NP: u32 = 5; // predicates 0..NP
const NL: u32 = 10; // literal objects LITERAL_FIRST_ID..+NL

fn objects() -> Vec<EntityLiteralId> {
    (0..NE + 2)
        .chain((0..NL + 2).map(|i| LITERAL_FIRST_ID + i))
        .collect()
}

fn random_triple(rng: &mut Rng) -> IdTriple {
    let s = rng.below(NS);
    let p = rng.below(NP);
    let o = if rng.below(4) == 0 {
        LITERAL_FIRST_ID + rng.below(NL)
    } else {
        rng.below(NE)
    };
    IdTriple::new(s, p, o)
}

fn random_store(rng: &mut Rng, n: usize) -> TripleStore {
    let mut s = TripleStore::new();
    MutableStore::bulk_load(&mut s, (0..n).map(|_| random_triple(rng)).collect());
    s
}

/// A triple to change: half the time an existing one of `oracle`.
fn pick(rng: &mut Rng, oracle: &TripleStore) -> IdTriple {
    if rng.below(2) == 0 && oracle.triple_count() > 0 {
        let all: Vec<IdTriple> = oracle.iter_all().collect();
        all[rng.below(all.len() as u32) as usize]
    } else {
        random_triple(rng)
    }
}

/// Every `TripleSource` method of `got` equals `want`'s, lists in order.
fn check_same<A: TripleSource, B: TripleSource>(got: &A, want: &B, ctx: &str) {
    let objs = objects();
    assert_eq!(
        got.triple_count(),
        want.triple_count(),
        "{ctx}: triple_count"
    );
    assert_eq!(
        got.subject_keys(),
        want.subject_keys(),
        "{ctx}: subject_keys"
    );
    assert_eq!(got.object_keys(), want.object_keys(), "{ctx}: object_keys");
    assert_eq!(
        got.distinct_subjects(),
        want.distinct_subjects(),
        "{ctx}: distinct_subjects"
    );
    assert_eq!(
        got.distinct_objects(),
        want.distinct_objects(),
        "{ctx}: distinct_objects"
    );
    assert_eq!(
        got.num_predicates(),
        want.num_predicates(),
        "{ctx}: num_predicates"
    );
    assert_eq!(got.iter_all(), want.iter_all(), "{ctx}: iter_all");
    for p in 0..NP + 1 {
        assert_eq!(got.so_by_p(p), want.so_by_p(p), "{ctx}: so_by_p({p})");
        assert_eq!(got.subs_by_p(p), want.subs_by_p(p), "{ctx}: subs_by_p({p})");
        assert_eq!(got.objs_by_p(p), want.objs_by_p(p), "{ctx}: objs_by_p({p})");
        assert_eq!(got.pred_card(p), want.pred_card(p), "{ctx}: pred_card({p})");
        assert_eq!(
            got.pred_distinct_subj(p),
            want.pred_distinct_subj(p),
            "{ctx}: pred_distinct_subj({p})"
        );
        assert_eq!(
            got.pred_distinct_obj(p),
            want.pred_distinct_obj(p),
            "{ctx}: pred_distinct_obj({p})"
        );
        assert_eq!(
            got.count_so_by_p(p),
            want.count_so_by_p(p),
            "{ctx}: count_so_by_p({p})"
        );
        assert_eq!(
            got.has_predicate(p),
            want.has_predicate(p),
            "{ctx}: has_predicate({p})"
        );
    }
    for s in 0..NS + 2 {
        assert_eq!(got.po_by_s(s), want.po_by_s(s), "{ctx}: po_by_s({s})");
        assert_eq!(
            got.count_po_by_s(s),
            want.count_po_by_s(s),
            "{ctx}: count_po_by_s({s})"
        );
        assert_eq!(
            got.distinct_p_of_s(s),
            want.distinct_p_of_s(s),
            "{ctx}: distinct_p_of_s({s})"
        );
        assert_eq!(
            got.has_subject(s),
            want.has_subject(s),
            "{ctx}: has_subject({s})"
        );
        for p in 0..NP + 1 {
            assert_eq!(got.o_by_sp(s, p), want.o_by_sp(s, p), "{ctx}: o_by_sp");
            assert_eq!(
                got.count_o_by_sp(s, p),
                want.count_o_by_sp(s, p),
                "{ctx}: count_o_by_sp"
            );
        }
        for &o in &objs {
            assert_eq!(got.p_by_so(s, o), want.p_by_so(s, o), "{ctx}: p_by_so");
            assert_eq!(
                got.count_p_by_so(s, o),
                want.count_p_by_so(s, o),
                "{ctx}: count_p_by_so"
            );
        }
    }
    for &o in &objs {
        assert_eq!(got.ps_by_o(o), want.ps_by_o(o), "{ctx}: ps_by_o({o})");
        assert_eq!(
            got.count_ps_by_o(o),
            want.count_ps_by_o(o),
            "{ctx}: count_ps_by_o({o})"
        );
        assert_eq!(
            got.distinct_p_of_o(o),
            want.distinct_p_of_o(o),
            "{ctx}: distinct_p_of_o({o})"
        );
        assert_eq!(
            got.has_object(o),
            want.has_object(o),
            "{ctx}: has_object({o})"
        );
        for p in 0..NP + 1 {
            assert_eq!(got.s_by_po(p, o), want.s_by_po(p, o), "{ctx}: s_by_po");
            assert_eq!(
                got.count_s_by_po(p, o),
                want.count_s_by_po(p, o),
                "{ctx}: count_s_by_po"
            );
            for s in 0..NS {
                assert_eq!(
                    got.exists(s, p, o),
                    want.exists(s, p, o),
                    "{ctx}: exists({s},{p},{o})"
                );
            }
        }
    }
}

/// A scratch `kvstore.kv` path unique to this process and tag.
fn tmp(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("gstore_dt_overlay_{tag}_{}.kv", std::process::id()));
    cleanup(&p);
    p
}

fn wal(p: &Path) -> PathBuf {
    let mut w = p.as_os_str().to_owned();
    w.push(".wal");
    PathBuf::from(w)
}

fn cleanup(p: &Path) {
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_file(wal(p));
}

fn ent(i: u32) -> String {
    format!("<http://example.org/e{i}>")
}
fn pred(i: u32) -> String {
    format!("<http://example.org/p{i}>")
}
fn lit(i: u32) -> String {
    format!("\"l{i}\"")
}

/// A disk store whose entity ids `0..NE+2`, predicate ids `0..NP+1` and
/// literal ids `LITERAL_FIRST_ID + 0..NL+2` are interned (in id order), with
/// no triple. `cache_pages` is small so the trees are paged.
fn seeded_store(path: &Path, cache_pages: usize) -> DiskStore {
    let mut ds = DiskStore::open(path, cache_pages).unwrap();
    let ents = NE + 2;
    let preds = NP + 1;
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
    assert_eq!(ds.entity_num() as u32, ents);
    assert_eq!(ds.predicate_num() as u32, preds);
    assert_eq!(ds.literal_num() as u32, NL + 2);
    assert_eq!(ds.triple_count(), 0);
    ds
}

fn pager_state(dt: &DiskTriples) -> (u32, usize) {
    let pager = dt.shared_pager();
    let pager = pager.read().unwrap();
    (pager.page_count(), pager.dirty_pages())
}

/// `check_same` inside a read scope that must stay error-free.
fn check_disk(dt: &DiskTriples, want: &TripleStore, ctx: &str) {
    let scope = ReadScope::enter();
    check_same(dt, want, ctx);
    assert_eq!(scope.take_error(), None, "{ctx}: a read error was noted");
}

// ---- the overlay model ----------------------------------------------------------

#[test]
fn overlay_model_matches_a_materialized_store() {
    for seed in 0..10u64 {
        let mut rng = Rng(0x5EED_0000 + seed);
        let base = random_store(&mut rng, 40 + 15 * seed as usize);
        let mut oracle = base.clone();
        let mut overlay = Overlay::new();
        for step in 0..360 {
            let t = pick(&mut rng, &oracle);
            if rng.below(2) == 0 {
                assert_eq!(
                    overlay.insert(&base, t),
                    oracle.insert(t),
                    "seed {seed} step {step}: insert flag for {t:?}"
                );
            } else {
                assert_eq!(
                    overlay.remove(&base, t),
                    oracle.remove(t),
                    "seed {seed} step {step}: remove flag for {t:?}"
                );
            }
            if step % 40 == 39 {
                check_same(
                    &overlay.view(&base),
                    &oracle,
                    &format!("seed {seed} step {step}"),
                );
            }
        }
        // The drained changes turn the base into the oracle.
        let changes = overlay.drain();
        assert!(overlay.is_empty());
        let mut applied = base.clone();
        let mut sorted = changes.removed.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, changes.removed, "removed is in (s, p, o) order");
        for t in &changes.removed {
            assert!(applied.remove(*t), "removed ⊆ base");
        }
        for t in &changes.added {
            assert!(applied.insert(*t), "added ∩ base = ∅");
        }
        check_same(&applied, &oracle, &format!("seed {seed} drained"));
    }
}

#[test]
fn overlay_reads_of_untouched_keys_go_to_the_base() {
    let mut rng = Rng(7);
    let base = random_store(&mut rng, 200);
    let mut overlay = Overlay::new();
    // Touch only subject NS + 1 (absent from the base).
    overlay.insert(&base, IdTriple::new(NS + 1, 0, 3));
    let view = overlay.view(&base);
    for s in 0..NS {
        assert_eq!(view.po_by_s(s), base.po_by_s(s).to_vec());
        assert_eq!(view.distinct_p_of_s(s), base.distinct_p_of_s(s));
    }
    assert_eq!(view.distinct_subjects(), base.distinct_subjects() + 1);
}

// ---- DiskTriples ------------------------------------------------------------------

#[test]
fn disk_transactions_match_the_model_and_never_touch_the_pager() {
    let path = tmp("model");
    let mut dt = DiskTriples::new(seeded_store(&path, 16)).unwrap();
    let mut rng = Rng(0xD15C);
    let mut oracle = random_store(&mut rng, 160);
    let base: Vec<IdTriple> = oracle.iter_all().collect();
    assert_eq!(
        dt.insert_many(base.iter().copied()).unwrap(),
        base.len() as u64
    );
    dt.commit_point().unwrap();
    check_disk(&dt, &oracle, "base");

    for round in 0..4 {
        let committed = oracle.clone();
        let mut txn_oracle = oracle.clone();
        let file_before = std::fs::read(&path).unwrap();
        let pager_before = pager_state(&dt);
        let epoch_before = dt.epoch();
        dt.begin().unwrap();
        for step in 0..150 {
            let t = pick(&mut rng, &txn_oracle);
            if rng.below(2) == 0 {
                assert_eq!(dt.insert_triple(t).unwrap(), txn_oracle.insert(t));
            } else {
                assert_eq!(dt.remove_triple(t).unwrap(), txn_oracle.remove(t));
            }
            if step % 50 == 49 {
                check_disk(&dt, &txn_oracle, &format!("round {round} step {step}"));
                assert_eq!(
                    pager_state(&dt),
                    pager_before,
                    "a transaction write reached the pager"
                );
                assert_eq!(dt.epoch(), epoch_before);
                // The committed state is untouched.
                check_same(
                    &DiskSource::new(dt.disk_store()),
                    &committed,
                    "committed state during the transaction",
                );
            }
        }
        assert_eq!(pager_state(&dt), pager_before);
        assert_eq!(std::fs::read(&path).unwrap(), file_before);
        if round % 2 == 0 {
            assert!(dt.rollback());
            assert_eq!(pager_state(&dt), pager_before, "rollback touched the pager");
            assert_eq!(dt.epoch(), epoch_before);
            check_disk(&dt, &committed, &format!("round {round} rolled back"));
        } else {
            dt.commit().unwrap();
            assert!(!dt.in_transaction());
            assert!(
                dt.epoch() > epoch_before,
                "a commit that changed data moves the epoch"
            );
            oracle = txn_oracle;
            check_disk(&dt, &oracle, &format!("round {round} committed"));
            dt.commit_point().unwrap();
        }
    }

    // Reopen: the committed transactions are durable.
    drop(dt);
    let dt = DiskTriples::open(&path, 16).unwrap();
    check_disk(&dt, &oracle, "reopened");
    drop(dt);
    cleanup(&path);
}

#[test]
fn commit_applies_named_ops_and_rollback_drops_them() {
    let path = tmp("named");
    let mut dt = DiskTriples::new(seeded_store(&path, 64)).unwrap();
    let (g, t) = (3, IdTriple::new(1, 2, 4));
    dt.begin().unwrap();
    dt.named_op(true, g, t).unwrap();
    assert_eq!(dt.transaction().unwrap().named_ops().len(), 1);
    assert!(dt.disk_store().quads().unwrap().is_empty());
    assert!(dt.rollback());
    assert!(dt.disk_store().quads().unwrap().is_empty());

    dt.begin().unwrap();
    dt.named_op(true, g, t).unwrap();
    dt.named_op(true, g, IdTriple::new(2, 2, 4)).unwrap();
    dt.named_op(false, g, IdTriple::new(2, 2, 4)).unwrap();
    assert!(dt.insert_triple(IdTriple::new(0, 0, 1)).unwrap());
    dt.commit().unwrap();
    assert_eq!(dt.disk_store().quads().unwrap(), vec![(g, t)]);
    assert_eq!(dt.triple_count(), 1);

    // Auto-commit named ops; a no-op leaves the epoch alone.
    let e = dt.epoch();
    dt.named_op(true, g, t).unwrap();
    assert_eq!(dt.epoch(), e);
    dt.named_op(false, g, t).unwrap();
    assert_eq!(dt.epoch(), e + 1);
    assert!(dt.disk_store().quads().unwrap().is_empty());
    drop(dt);
    cleanup(&path);
}

#[test]
fn views_expire_after_a_write_and_close_after_drop() {
    let path = tmp("views");
    let mut dt = DiskTriples::new(seeded_store(&path, 64)).unwrap();
    let t1 = IdTriple::new(0, 0, 1);
    let t2 = IdTriple::new(1, 0, 2);
    assert!(dt.insert_triple(t1).unwrap());
    let count = |v: &DiskReadView| v.with_base(|ds, _| DiskSource::new(ds).triple_count());

    let view = dt.read_view();
    assert_eq!(count(&view).unwrap(), 1);
    assert!(view.is_current());

    // A no-op write changes nothing: the view stays valid.
    assert!(!dt.insert_triple(t1).unwrap());
    assert_eq!(count(&view).unwrap(), 1);

    // A transaction does not touch the committed state until it commits, and
    // the view never sees its writes.
    dt.begin().unwrap();
    assert!(dt.insert_triple(t2).unwrap());
    assert_eq!(dt.triple_count(), 2, "read-your-writes");
    assert_eq!(
        count(&view).unwrap(),
        1,
        "a view never sees an open transaction"
    );
    dt.commit().unwrap();
    assert!(matches!(count(&view), Err(ViewError::Expired)));
    assert!(!view.is_current());

    let view = dt.read_view();
    assert_eq!(count(&view).unwrap(), 2);
    assert!(dt.remove_triple(t2).unwrap());
    assert!(matches!(count(&view), Err(ViewError::Expired)));

    let view = dt.read_view();
    assert_eq!(count(&view).unwrap(), 1);
    drop(dt);
    assert!(matches!(count(&view), Err(ViewError::Closed)));
    cleanup(&path);
}

#[test]
fn broken_state_refuses_writes_and_reads_until_reopen() {
    let path = tmp("broken");
    let mut dt = DiskTriples::new(seeded_store(&path, 64)).unwrap();
    let mut rng = Rng(99);
    let base: Vec<IdTriple> = (0..60).map(|_| random_triple(&mut rng)).collect();
    dt.insert_many(base.iter().copied()).unwrap();
    dt.commit_point().unwrap();
    let durable = dt.triple_count();
    let view = dt.read_view();

    // A transaction whose commit fails after two of its pager writes.
    dt.begin().unwrap();
    let mut added = 0;
    for i in 0..10 {
        added += u32::from(dt.insert_triple(IdTriple::new(NS + 1, 1, i)).unwrap());
    }
    assert_eq!(added, 10);
    dt.debug_fail_write_after(2);
    let err = dt.commit().unwrap_err().to_string();
    assert!(err.contains("injected"), "{err}");
    assert!(dt.broken().is_some());
    assert!(!dt.in_transaction());

    // Writes are refused...
    assert!(dt.insert_triple(IdTriple::new(0, 0, 0)).is_err());
    assert!(dt.begin().is_err());
    assert!(dt.named_op(true, 0, IdTriple::new(0, 0, 0)).is_err());
    assert!(!MutableStore::insert(&mut dt, IdTriple::new(0, 0, 0)));
    assert!(dt.take_error().unwrap().contains("injected"));
    assert!(dt.commit_point().is_err());
    // ... reads fail the statement ...
    let scope = ReadScope::enter();
    assert_eq!(dt.triple_count(), 0);
    assert!(dt.so_by_p(1).is_empty());
    assert!(dt.has_subject(0), "has_* answer true when in doubt");
    assert!(scope.take_error().unwrap().contains("broken"));
    drop(scope);
    // ... and so do views.
    assert!(matches!(
        view.with_base(|_, _| ()),
        Err(ViewError::Io(e)) if e.to_string().contains("broken")
    ));
    assert!(matches!(
        dt.read_view().with_base(|_, _| ()),
        Err(ViewError::Io(_))
    ));

    // Dropping a broken store flushes nothing: reopening finds the state of
    // the last commit point, without the half-applied commit.
    drop(dt);
    let dt = DiskTriples::open(&path, 64).unwrap();
    assert!(dt.broken().is_none());
    assert_eq!(dt.triple_count(), durable);
    assert!(dt.po_by_s(NS + 1).is_empty());
    drop(dt);

    // A failing commit point breaks the store too.
    let mut dt = DiskTriples::open(&path, 64).unwrap();
    assert!(dt.insert_triple(IdTriple::new(NS + 1, 1, 1)).unwrap());
    dt.debug_fail_next_write();
    assert!(dt.commit_point().is_err());
    assert!(dt.broken().is_some());
    drop(dt);
    let dt = DiskTriples::open(&path, 64).unwrap();
    assert_eq!(dt.triple_count(), durable);
    drop(dt);
    cleanup(&path);
}

#[test]
fn drop_flushes_applied_writes_and_drops_an_open_transaction() {
    let path = tmp("drop");
    let mut dt = DiskTriples::new(seeded_store(&path, 64)).unwrap();
    assert!(dt.insert_triple(IdTriple::new(0, 0, 1)).unwrap());
    assert!(dt.is_dirty());
    dt.begin().unwrap();
    assert!(dt.insert_triple(IdTriple::new(1, 0, 1)).unwrap());
    drop(dt); // no commit point: Drop commits the auto-committed write
    let mut dt = DiskTriples::open(&path, 64).unwrap();
    assert_eq!(dt.iter_all(), vec![IdTriple::new(0, 0, 1)]);

    // close() reports the same as drop does, and refuses nothing it should not.
    assert!(dt.insert_triple(IdTriple::new(2, 0, 1)).unwrap());
    dt.close().unwrap();
    let dt = DiskTriples::open(&path, 64).unwrap();
    assert_eq!(dt.triple_count(), 2);
    drop(dt);
    cleanup(&path);
}

/// Overwrite the root page of the tree in header slot `slot` with garbage.
fn corrupt_root(path: &Path, slot: usize) {
    let mut bytes = std::fs::read(path).unwrap();
    let off = 16 + slot * 8;
    let page = u64::from_le_bytes(bytes[off..off + 8].try_into().unwrap()) as usize;
    assert!(page > 0, "the tree has a root page");
    bytes[page * 4096..(page + 1) * 4096].fill(0xFF);
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn read_errors_reach_the_scope_and_stay_on_their_thread() {
    const POS: usize = 1; // header slot of the POS tree
    let path = tmp("readerr");
    let mut dt = DiskTriples::new(seeded_store(&path, 64)).unwrap();
    let mut rng = Rng(5);
    let base: Vec<IdTriple> = (0..80).map(|_| random_triple(&mut rng)).collect();
    dt.insert_many(base).unwrap();
    let want_po: Vec<Vec<(u32, u32)>> = (0..NS).map(|s| dt.po_by_s(s)).collect();
    dt.close().unwrap();
    corrupt_root(&path, POS);
    let dt = Arc::new(DiskTriples::open(&path, 64).unwrap());

    // The error fails the statement that read.
    {
        let scope = ReadScope::enter();
        assert!(dt.so_by_p(0).is_empty());
        assert!(dt.has_predicate(0), "has_* answer true on a read error");
        assert!(scope.take_error().unwrap().contains("kvstore read failed"));
    }
    // Through an evaluator: the query's scope records it.
    {
        let dict = dt.disk_store().dictionary().unwrap();
        let q =
            gstore::parser::sparql::parse(&format!("SELECT ?s ?o WHERE {{ ?s {} ?o }}", pred(1)))
                .unwrap();
        let scope = ReadScope::enter();
        let _ = Evaluator::new(&dict, &*dt).evaluate(&q);
        assert!(scope.has_error(), "the query noted the read error");
    }
    // Through a snapshot view and a DiskSource.
    {
        let scope = ReadScope::enter();
        let n = dt
            .read_view()
            .with_base(|ds, _| DiskSource::new(ds).s_by_po(0, 1).len())
            .unwrap();
        assert_eq!(n, 0);
        assert!(scope.has_error());
    }

    // Isolation: a thread whose reads fail never fails another thread's
    // statement, though both read the same store at the same time.
    let start = Barrier::new(2);
    std::thread::scope(|sc| {
        let failing = sc.spawn(|| {
            start.wait();
            let mut errors = 0;
            for _ in 0..300 {
                let scope = ReadScope::enter();
                let _ = dt.so_by_p(0);
                errors += u32::from(scope.take_error().is_some());
            }
            errors
        });
        let healthy = sc.spawn(|| {
            start.wait();
            let mut errors = 0;
            for round in 0..300 {
                let scope = ReadScope::enter();
                let s = round % NS;
                assert_eq!(dt.po_by_s(s), want_po[s as usize]);
                errors += u32::from(scope.take_error().is_some());
            }
            errors
        });
        assert_eq!(failing.join().unwrap(), 300);
        assert_eq!(healthy.join().unwrap(), 0, "an error leaked across threads");
    });
    drop(dt);
    cleanup(&path);
}

// ---- LUBM through DiskTriples ------------------------------------------------------

fn lubm_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/lubm")
}

/// Ordered rows and the plan's partial-solution count.
fn run<S: TripleSource>(eval: Evaluator<'_, S>, q: &str) -> (Vec<Vec<Option<String>>>, u64) {
    let parsed = gstore::parser::sparql::parse(q).unwrap();
    let scope = ReadScope::enter();
    let rows = match eval.evaluate(&parsed).unwrap() {
        QueryResult::Select(rs) => rs.rows,
        QueryResult::Ask(b) => vec![vec![Some(b.to_string())]],
        other => panic!("unexpected result {other:?}"),
    };
    assert_eq!(scope.take_error(), None, "read error in {q}");
    (rows, eval.stats().bgp_bindings)
}

#[test]
fn lubm_queries_over_disk_triples_equal_the_materialized_store() {
    let path = tmp("lubm");
    let ds = DiskStore::build_files(&path, 1024, &[lubm_dir().join("lubm.nt")]).unwrap();
    let (dict, mut store) = ds.to_memory().unwrap();
    let mut dt = DiskTriples::new(ds).unwrap();
    let queries: Vec<String> = (1..=14)
        .map(|i| std::fs::read_to_string(lubm_dir().join(format!("lubm_q{i}.rq"))).unwrap())
        .collect();
    let compare = |dt: &DiskTriples, store: &TripleStore, ctx: &str| {
        for (i, q) in queries.iter().enumerate() {
            let disk = run(Evaluator::new(&dict, dt), q);
            let mem = run(Evaluator::new(&dict, store), q);
            assert_eq!(disk.0, mem.0, "{ctx}: Q{} rows differ", i + 1);
            assert_eq!(disk.1, mem.1, "{ctx}: Q{} plan differs", i + 1);
        }
    };
    compare(&dt, &store, "committed");

    // An open transaction that copies a student's courses to other students,
    // deletes some memberships and types: the evaluator sees exactly the
    // materialized result of the same changes, in the same order.
    let ub = "http://www.lehigh.edu/~zhp2/2004/0401/univ-bench.owl#";
    let pid = |name: &str| dict.predicate_id(&format!("<{ub}{name}>")).unwrap();
    let (takes, member, rdf_type) = (
        pid("takesCourse"),
        pid("memberOf"),
        dict.predicate_id("<http://www.w3.org/1999/02/22-rdf-syntax-ns#type>")
            .unwrap(),
    );
    let takes_pairs = store.so_by_p(takes).to_vec();
    let member_pairs = store.so_by_p(member).to_vec();
    let type_pairs = store.so_by_p(rdf_type).to_vec();
    dt.begin().unwrap();
    let mut rng = Rng(1234);
    for _ in 0..400 {
        let (s, _) = takes_pairs[rng.below(takes_pairs.len() as u32) as usize];
        let (_, c) = takes_pairs[rng.below(takes_pairs.len() as u32) as usize];
        let t = IdTriple::new(s, takes, c);
        assert_eq!(dt.insert_triple(t).unwrap(), store.insert(t));
    }
    for _ in 0..200 {
        let (s, o) = member_pairs[rng.below(member_pairs.len() as u32) as usize];
        let t = IdTriple::new(s, member, o);
        assert_eq!(dt.remove_triple(t).unwrap(), store.remove(t));
        let (s, o) = type_pairs[rng.below(type_pairs.len() as u32) as usize];
        let t = IdTriple::new(s, rdf_type, o);
        assert_eq!(dt.remove_triple(t).unwrap(), store.remove(t));
    }
    compare(&dt, &store, "open transaction");
    dt.commit().unwrap();
    compare(&dt, &store, "after commit");
    drop(dt);
    cleanup(&path);
}

// ---- snapshot readers against a committer -------------------------------------------

/// The predicate pair of the stress test: every commit adds one `(s, P_A, O)`
/// and one `(s, P_B, O)`, so a consistent state has as many of each.
const P_A: u32 = 0;
const P_B: u32 = 1;
const O_PAIR: u32 = 2;

struct StressReport {
    commits: Vec<Duration>,
    expired: u64,
    completed: u64,
}

/// `readers` snapshot readers run "queries" that loop until cancelled (or
/// for at most `query_time`) while one committer commits `commits` pairs,
/// pausing `pause` between commits. Checks what every reader saw.
fn stress(
    tag: &str,
    readers: usize,
    commits: u32,
    pause: Duration,
    query_time: Duration,
) -> StressReport {
    let path = tmp(tag);
    let mut dt = DiskTriples::new(seeded_store(&path, 256)).unwrap();
    let initial = 50u32;
    for s in 0..initial {
        dt.insert_many([
            IdTriple::new(1000 + s, P_A, O_PAIR),
            IdTriple::new(1000 + s, P_B, O_PAIR),
        ])
        .unwrap();
    }
    let dt = RwLock::new(dt);
    let stop = AtomicBool::new(false);
    let committed = Mutex::new(vec![u64::from(initial)]);
    let expired = AtomicU64::new(0);
    let completed = AtomicU64::new(0);
    let start = Barrier::new(readers + 1);

    let reader = || {
        start.wait();
        let mut last_seen = 0u64;
        loop {
            let finishing = stop.load(Ordering::SeqCst);
            let view = dt.read().unwrap().read_view();
            let scope = ReadScope::enter();
            let began = Instant::now();
            let r = view.with_base(|ds, cancel| {
                let src = DiskSource::new(ds);
                let mut seen: Option<u64> = None;
                loop {
                    // One "query step": a few reads that must agree.
                    let a = src.count_so_by_p(P_A);
                    let b = src.so_by_p(P_B).len() as u64;
                    let total = src.triple_count();
                    assert_eq!(a, b, "torn state: {a} vs {b}");
                    assert_eq!(total, 2 * a, "torn state: {total} triples, {a} pairs");
                    if let Some(prev) = seen {
                        assert_eq!(prev, a, "the state changed under a reader");
                    }
                    seen = Some(a);
                    if cancel.load(Ordering::SeqCst)
                        || stop.load(Ordering::SeqCst)
                        || began.elapsed() >= query_time
                    {
                        break;
                    }
                    std::thread::yield_now();
                }
                seen.unwrap()
            });
            assert_eq!(scope.take_error(), None);
            match r {
                Ok(pairs) => {
                    assert!(pairs >= last_seen, "a reader went back in time");
                    assert!(
                        committed.lock().unwrap().contains(&pairs),
                        "{pairs} pairs is not a committed state"
                    );
                    last_seen = pairs;
                    completed.fetch_add(1, Ordering::SeqCst);
                    if finishing {
                        break pairs;
                    }
                }
                Err(ViewError::Expired) => {
                    expired.fetch_add(1, Ordering::SeqCst);
                }
                Err(e) => panic!("unexpected view error {e}"),
            }
        }
    };

    let mut commit_times = Vec::new();
    std::thread::scope(|sc| {
        let handles: Vec<_> = (0..readers).map(|_| sc.spawn(reader)).collect();
        start.wait();
        for i in 0..commits {
            std::thread::sleep(pause);
            let mut w = dt.write().unwrap();
            w.begin().unwrap();
            w.insert_triple(IdTriple::new(5000 + i, P_A, O_PAIR))
                .unwrap();
            w.insert_triple(IdTriple::new(5000 + i, P_B, O_PAIR))
                .unwrap();
            // Announced before the commit: a reader may see it as soon as
            // `commit` returns.
            committed.lock().unwrap().push(u64::from(initial + i + 1));
            let t = Instant::now();
            w.commit().unwrap();
            commit_times.push(t.elapsed());
            if i % 4 == 3 {
                w.commit_point().unwrap();
            }
        }
        stop.store(true, Ordering::SeqCst);
        let finals: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        // Readers converge on the committed state.
        for f in finals {
            assert_eq!(f, u64::from(initial + commits));
        }
    });
    drop(dt);
    cleanup(&path);
    StressReport {
        commits: commit_times,
        expired: expired.load(Ordering::SeqCst),
        completed: completed.load(Ordering::SeqCst),
    }
}

#[test]
fn snapshot_readers_never_see_a_torn_state_and_commits_cancel_them() {
    let report = stress(
        "stress",
        8,
        12,
        Duration::from_millis(60),
        Duration::from_secs(10),
    );
    let worst = report.commits.iter().max().copied().unwrap();
    eprintln!(
        "commit times {:?}; {} expired, {} completed reads",
        report.commits, report.expired, report.completed
    );
    assert!(
        worst <= Duration::from_millis(200),
        "a commit took {worst:?} to cancel and drain the readers: {:?}",
        report.commits
    );
    assert!(report.expired > 0, "no reader was ever cancelled");
    assert!(report.completed >= 8);
}

/// The long variant: 10-second "queries", commits every 2 s for about a
/// minute. Run with `cargo test --test dt_overlay -- --ignored`.
#[test]
#[ignore]
fn snapshot_soak_with_long_queries() {
    let report = stress(
        "soak",
        8,
        30,
        Duration::from_secs(2),
        Duration::from_secs(10),
    );
    let worst = report.commits.iter().max().copied().unwrap();
    assert!(worst <= Duration::from_millis(200), "{:?}", report.commits);
    assert!(report.expired >= 30);
}
