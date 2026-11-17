//! `ConcurrentDb` (the library's snapshot-isolation + OCC wrapper): the cost
//! of its version-chain garbage collection, updates that fail part-way, and
//! panics in its writer paths (review conc#2, http#8, conc#4, http#9,
//! robust#13, conc#3, http#17, conc#15).
//!
//! Every test here uses the public API only. The panics *inside* a commit or
//! an UPDATE (which no public call can provoke) are driven by test-only
//! failpoints in `src/concurrent.rs`'s own test module.

use std::panic::{catch_unwind, AssertUnwindSafe};

use gstore::concurrent::{ConcurrentDb, IsolationLevel, VersionChainStats};
use gstore::model::{Term, Triple};
use gstore::{GStoreError, QueryResult};

const DATA: &str = "<http://ex/s> <http://ex/p> <http://ex/o> .\n";

fn cdb() -> ConcurrentDb {
    ConcurrentDb::build_from_str("conc", DATA).unwrap()
}

fn t(s: &str, o: &str) -> Triple {
    Triple::new(
        Term::iri(format!("http://ex/{s}")),
        Term::iri("http://ex/p"),
        Term::iri(format!("http://ex/{o}")),
    )
}

/// `ASK { <s> <p> <o> }` against the latest published snapshot.
fn published(c: &ConcurrentDb, s: &str, o: &str) -> bool {
    let q = format!("ASK {{ <http://ex/{s}> <http://ex/p> <http://ex/{o}> }}");
    match c.query(&q).unwrap() {
        QueryResult::Ask(b) => b,
        other => panic!("expected ASK, got {other:?}"),
    }
}

fn is_conflict<T: std::fmt::Debug>(r: &gstore::Result<T>) -> bool {
    matches!(r, Err(GStoreError::Conflict(_)))
}

fn stats(c: &ConcurrentDb) -> VersionChainStats {
    c.version_chain_stats()
}

/// One transaction inserting one new triple, committed.
fn commit_insert(c: &ConcurrentDb, i: usize) {
    let mut txn = c.begin();
    txn.insert(t(&format!("k{i}"), "v"));
    txn.commit().unwrap();
}

// ---- version-chain GC cost (conc#2, http#8) --------------------------------

/// The GC work of a commit does not grow with the number of chains earlier
/// commits left behind. Every transactionally inserted triple keeps a
/// one-record chain (its state differs from the construction-time base), and
/// the GC used to walk all of them on every commit, abort, `Txn` drop and
/// `VersionView` drop: 20,000 commits later each of those examined 20,000
/// chains (8 ms per commit, quadratic in total). Now a GC examines only the
/// chains of the versions the GC floor has passed since the last one.
#[test]
fn version_chain_gc_work_stays_flat_after_20k_commits() {
    const WARM: usize = 20_000;
    const PROBE: usize = 100;
    let c = cdb();

    let before = stats(&c);
    for i in 0..PROBE {
        commit_insert(&c, i);
    }
    let early = stats(&c).gc_visits - before.gc_visits;

    for i in PROBE..WARM {
        commit_insert(&c, i);
    }
    let warm = stats(&c);
    assert_eq!(warm.chains, WARM, "one retained chain per inserted key");
    assert_eq!(warm.records, WARM, "each collapsed to one record");

    for i in WARM..WARM + PROBE {
        commit_insert(&c, i);
    }
    let late = stats(&c).gc_visits - warm.gc_visits;
    // About one chain per one-triple commit, before and after.
    assert!(early <= 2 * PROBE as u64, "early: {early} visits");
    assert!(
        late <= early + PROBE as u64,
        "the GC work of {PROBE} commits grew from {early} to {late} chain visits after \
         {WARM} commits"
    );

    // Nothing to collect: an empty transaction, an abort and a version view
    // let go examine no chain at all.
    let idle = stats(&c).gc_visits;
    for _ in 0..PROBE {
        drop(c.begin());
        let mut aborted = c.begin();
        aborted.insert(t("never", "committed"));
        aborted.abort();
        drop(c.version_view());
    }
    assert_eq!(stats(&c).gc_visits, idle, "idle drops did chain GC work");

    // A conflicting commit (an abort) costs no more than a commit.
    let mut loser = c.begin();
    loser.insert(t("k0", "v"));
    commit_insert(&c, 0);
    let at = stats(&c).gc_visits;
    assert!(is_conflict(&loser.commit()));
    assert!(stats(&c).gc_visits - at <= 2);
}

/// Chains whose committed state is back to the base snapshot's are still
/// reclaimed (incrementally): a churn of inserts and deletes leaves no chain,
/// and the work is one visit per written record.
#[test]
fn churned_keys_leave_no_chains() {
    const KEYS: usize = 2_000;
    let c = cdb();
    for i in 0..KEYS {
        commit_insert(&c, i);
    }
    assert_eq!(stats(&c).chains, KEYS);
    for i in 0..KEYS {
        let mut txn = c.begin();
        txn.delete(t(&format!("k{i}"), "v"));
        txn.commit().unwrap();
    }
    let s = stats(&c);
    assert_eq!((s.chains, s.records), (0, 0), "{s:?}");
    assert!(s.gc_visits <= 2 * 2 * KEYS as u64, "{s:?}");
    // The base triple is untouched; the churned ones are gone.
    assert!(published(&c, "s", "o"));
    assert!(!published(&c, "k7", "v"));
}

/// A version view pinned while commits go on keeps seeing its version; the
/// records it needs survive the incremental GC until it is dropped, and
/// then collapse.
#[test]
fn a_pinned_view_keeps_its_records_until_dropped() {
    let c = cdb();
    commit_insert(&c, 1);
    let view = c.version_view();
    let mut txn = c.begin();
    txn.delete(t("k1", "v"));
    txn.insert(t("k2", "v"));
    txn.commit().unwrap();
    for i in 3..50 {
        commit_insert(&c, i);
    }
    assert!(view.contains(&t("k1", "v")), "deleted after the pin");
    assert!(!view.contains(&t("k2", "v")), "inserted after the pin");
    let pinned = stats(&c);
    assert_eq!(pinned.records, pinned.chains + 1, "k1: insert + tombstone");
    drop(view);
    // The next GC (any commit or drop) collapses k1's chain: back to the base.
    drop(c.begin());
    let s = stats(&c);
    assert_eq!(s.chains, 48, "k2..k49: {s:?}");
    assert_eq!(s.records, s.chains);
    assert!(!c.version_view().contains(&t("k1", "v")));
}

// ---- UPDATE failing part-way (conc#4, http#9, robust#13) -------------------

/// An UPDATE whose later operation fails keeps its earlier operations
/// applied (as `Database::query` does). They are published as a new version
/// — readers see what the writer database holds — and that version counts
/// as a non-transactional write: a transaction that overlaps it conflicts
/// instead of committing as if nothing had happened.
#[test]
fn a_partially_applied_update_is_published_and_conflicts_overlapping_txns() {
    for level in [IsolationLevel::RepeatableRead, IsolationLevel::Serializable] {
        let c = cdb();
        let mut txn = c.begin_with(level);
        assert!(!txn.read(&t("a", "b")));
        txn.delete(t("a", "b"));
        let v = c.version();

        let r = c.update(
            "INSERT DATA { <http://ex/a> <http://ex/p> <http://ex/b> } ; \
             ADD <http://ex/nograph> TO DEFAULT",
        );
        assert!(r.is_err(), "the ADD fails: {r:?}");
        assert!(
            published(&c, "a", "b"),
            "the applied INSERT is published ({level:?})"
        );
        assert_eq!(c.version(), v + 1, "as a new version ({level:?})");
        let r = txn.commit();
        assert!(
            is_conflict(&r),
            "a {level:?} transaction overlapping the partial update must conflict: {r:?}"
        );
        assert!(
            published(&c, "a", "b"),
            "the conflicting delete was not applied"
        );
    }
}

/// An UPDATE that fails before changing anything publishes nothing and does
/// not conflict transactions (no spurious version).
#[test]
fn a_failed_update_that_changed_nothing_is_not_a_write() {
    let c = cdb();
    let mut txn = c.begin();
    txn.insert(t("x", "y"));
    let v = c.version();
    assert!(c.update("ADD <http://ex/nograph> TO DEFAULT").is_err());
    assert!(c.update("INSERT DATA { <http://ex/a> <bad iri> }").is_err());
    assert_eq!(c.version(), v);
    assert_eq!(txn.commit().unwrap(), v + 1);
    assert!(published(&c, "x", "y"));
}

// ---- panics in the writer paths (conc#3, http#17) ---------------------------

/// A `write()` closure that panics does not poison the wrapper: the panic
/// reaches the caller, what the closure applied before it is published and
/// counts as a non-transactional write (an overlapping transaction
/// conflicts), and later updates, syncs, commits and reads all work.
#[test]
fn a_panicking_write_closure_does_not_poison_the_writer() {
    let c = cdb();
    let mut overlapping = c.begin();
    overlapping.insert(t("q", "r"));
    let v = c.version();

    let r = catch_unwind(AssertUnwindSafe(|| {
        c.write(|db| {
            db.insert_triple(&t("half", "done"));
            panic!("a user closure panics");
        })
    }));
    assert!(r.is_err(), "the closure's panic reaches the caller");
    assert!(
        published(&c, "half", "done"),
        "its applied part is published"
    );
    assert_eq!(c.version(), v + 1);

    c.update("INSERT DATA { <http://ex/u> <http://ex/p> <http://ex/v> }")
        .expect("update after a panicking closure");
    c.sync().expect("sync after a panicking closure");
    let mut later = c.begin();
    later.insert(t("w", "x"));
    later.commit().expect("commit after a panicking closure");
    assert_eq!(c.write(|db| db.triple_num()), 4);
    assert!(published(&c, "u", "v") && published(&c, "w", "x"));
    assert!(is_conflict(&overlapping.commit()));
}

/// A database transaction a panicking `write()` closure opened is rolled
/// back: nothing of it is applied, and the next commit is not swallowed by
/// the transaction left open.
#[test]
fn a_transaction_left_open_by_a_panicking_closure_is_rolled_back() {
    let c = cdb();
    let r = catch_unwind(AssertUnwindSafe(|| {
        c.write(|db| {
            db.begin().unwrap();
            db.insert_triple(&t("in", "txn"));
            panic!("mid-transaction");
        })
    }));
    assert!(r.is_err());
    assert!(!c.write(|db| db.in_transaction()), "rolled back");
    assert!(!published(&c, "in", "txn"));
    let mut txn = c.begin();
    txn.insert(t("after", "panic"));
    txn.commit().unwrap();
    assert!(published(&c, "after", "panic"));
    assert_eq!(c.write(|db| db.triple_num()), 2);
}

/// A closure that opened a database transaction and returned normally is
/// left alone (a caller may batch several `write()` calls into one).
#[test]
fn a_transaction_a_closure_leaves_open_on_return_stays_open() {
    let c = cdb();
    c.write(|db| {
        db.begin().unwrap();
        db.insert_triple(&t("open", "txn"));
    });
    assert!(c.write(|db| db.in_transaction()));
    c.write(|db| db.commit().unwrap());
    assert!(published(&c, "open", "txn"));
}
