//! Concurrent access with snapshot isolation — a scoped analogue of gStore's
//! `Txn_manager` + MVCC.
//!
//! [`ConcurrentDb`] lets many reader threads query while writers commit, with
//! **snapshot isolation**: a query runs against an immutable, consistent
//! [`Snapshot`] (an `Arc`), so readers never observe a half-applied write and
//! never block writers or each other. The legacy `update()`/`write()` path
//! serializes writers through a [`Mutex`], mutates the authoritative
//! [`Database`], then publishes a fresh snapshot by atomically swapping the
//! `Arc` — in-flight readers keep their old snapshot.
//!
//! On top of that, [`ConcurrentDb::begin`] starts an **optimistic transaction**
//! ([`Txn`]) for *multi-writer* concurrency control (OCC, first-committer-wins):
//!
//! * `begin()` captures the current snapshot version `V` and buffers writes
//!   locally — it touches no shared state, so many txns proceed in parallel.
//! * `txn.insert` / `txn.delete` record into the buffer; `txn.query` reads the
//!   captured snapshot `V`, and `txn.contains` additionally reflects the txn's
//!   own buffered writes (read-your-writes).
//! * `txn.commit()` validates under the writer lock: if any version in
//!   `(V, current]` wrote a triple key this txn also writes, it aborts with
//!   [`GStoreError::Conflict`]. Otherwise it applies the buffer to the
//!   authoritative [`Database`], publishes a new snapshot, and records this
//!   version's written-key set for future validators.
//! * `run_txn` re-runs a closure on conflict, up to a caller-chosen attempt cap.
//!
//! A bounded history of `version → written-key-set` backs validation; entries
//! older than the oldest live transaction's start version are garbage-collected
//! on every commit/abort, so the history never grows unbounded.
//!
//! Cost model: publishing a snapshot clones the dictionary + triple indexes
//! (`O(store)` per commit). That keeps published snapshots immutable and the
//! implementation simple; finer-grained MVCC (per-key version chains,
//! persistent/shared structures) would avoid the copy.
//!
//! Scope note: OCC validation is among [`Txn`] commits. The legacy
//! `update()`/`write()` path still works and stays snapshot-consistent for
//! readers, but it does not record a write-key set, so it is not conflict-checked
//! against concurrent [`Txn`]s — treat the two write paths as separate.
//!
//! NOT done (see `docs/REFACTOR_BACKLOG.md` E): per-key version chains,
//! lock-free reads beyond the `Arc`-swap, deadlock detection, and snapshot GC
//! beyond `Arc` refcounting.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use crate::db::Database;
use crate::dict::Dictionary;
use crate::error::{GStoreError, Result};
use crate::model::Triple;
use crate::parser::sparql;
use crate::query::{Evaluator, QueryResult};
use crate::store::TripleStore;

/// A canonical, dictionary-independent key for a triple: its N-Triples surface
/// form (which round-trips losslessly), used for write-write conflict detection.
type TripleKey = String;

/// An immutable, consistent view of the data at one committed version.
pub struct Snapshot {
    dict: Dictionary,
    store: TripleStore,
    named: BTreeMap<u32, TripleStore>,
    version: u64,
}

impl Snapshot {
    fn from_db(db: &Database, version: u64) -> Snapshot {
        Snapshot {
            dict: db.dict().clone(),
            store: db.store().clone(),
            named: db.named_graphs().clone(),
            version,
        }
    }

    /// This snapshot's monotonically-increasing version number.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Whether this snapshot's default graph contains `t`. Used for txn
    /// read-your-writes point reads. Returns `false` if any term is unknown.
    pub fn contains(&self, t: &Triple) -> bool {
        let (Some(s), Some(p), Some(o)) = (
            self.dict.entity_id(&t.subject.dict_key()),
            self.dict.predicate_id(&t.predicate.dict_key()),
            self.dict.term_id(&t.object),
        ) else {
            return false;
        };
        self.store.exists(s, p, o)
    }

    /// Run a read query (SELECT/ASK/CONSTRUCT/DESCRIBE) against this snapshot.
    pub fn query(&self, sparql: &str) -> Result<QueryResult> {
        let q = sparql::parse(sparql)?;
        let mut eval = Evaluator::new(&self.dict, &self.store);
        if !self.named.is_empty() {
            eval = eval.with_named(&self.named);
        }
        eval.evaluate(&q)
    }
}

/// One buffered transaction write (default graph only).
enum TxnOp {
    Insert(Triple),
    Delete(Triple),
}

/// OCC bookkeeping shared by all transactions, guarded by a single mutex.
struct OccState {
    /// `version → set of triple keys written at that commit`. Validation scans
    /// the range `(txn.start, current]`; entries below the oldest live txn start
    /// are GC'd.
    history: BTreeMap<u64, HashSet<TripleKey>>,
    /// `start_version → number of live transactions started at that version`.
    /// The minimum key is the GC floor: history at/below it is unreachable.
    live_starts: BTreeMap<u64, usize>,
}

impl OccState {
    fn new() -> OccState {
        OccState {
            history: BTreeMap::new(),
            live_starts: BTreeMap::new(),
        }
    }

    /// Register a transaction that started at `version`.
    fn register(&mut self, version: u64) {
        *self.live_starts.entry(version).or_insert(0) += 1;
    }

    /// Deregister a finished (committed or aborted) transaction.
    fn deregister(&mut self, version: u64) {
        if let Some(c) = self.live_starts.get_mut(&version) {
            *c -= 1;
            if *c == 0 {
                self.live_starts.remove(&version);
            }
        }
    }

    /// Drop history entries no live transaction can still need. A txn that
    /// started at `s` only validates versions `> s`, so anything at or below the
    /// smallest live start is unreachable; with no live txns, all of it is.
    fn gc(&mut self) {
        let floor = self.live_starts.keys().next().copied().unwrap_or(u64::MAX);
        self.history.retain(|&v, _| v > floor);
    }
}

/// A thread-safe database wrapper offering concurrent snapshot-isolated reads,
/// optimistic multi-writer transactions, and the legacy serialized writer path.
/// Share it as `Arc<ConcurrentDb>` across threads.
pub struct ConcurrentDb {
    /// The authoritative mutable database; writers lock this (serialized).
    writer: Mutex<Database>,
    /// The currently-published immutable snapshot for readers.
    current: RwLock<Arc<Snapshot>>,
    /// Optimistic-concurrency bookkeeping (commit history + live-txn registry).
    occ: Mutex<OccState>,
}

impl ConcurrentDb {
    /// Wrap an existing [`Database`].
    pub fn new(db: Database) -> ConcurrentDb {
        let snap = Arc::new(Snapshot::from_db(&db, 1));
        ConcurrentDb {
            writer: Mutex::new(db),
            current: RwLock::new(snap),
            occ: Mutex::new(OccState::new()),
        }
    }

    /// Build from an in-memory RDF document.
    pub fn build_from_str(name: &str, content: &str) -> Result<ConcurrentDb> {
        Ok(ConcurrentDb::new(Database::build_from_str(name, content)?))
    }

    /// The current committed version number.
    pub fn version(&self) -> u64 {
        self.current.read().unwrap().version
    }

    /// Acquire a stable snapshot handle. Subsequent commits do not affect it, so
    /// a sequence of reads against the same snapshot is consistent.
    pub fn snapshot(&self) -> Arc<Snapshot> {
        Arc::clone(&self.current.read().unwrap())
    }

    /// Run a read query against the latest committed snapshot — without holding
    /// any lock during evaluation, so reads run fully concurrently.
    pub fn query(&self, sparql: &str) -> Result<QueryResult> {
        self.snapshot().query(sparql)
    }

    /// Apply a SPARQL UPDATE atomically: mutate the authoritative database (under
    /// the writer lock), then publish a new snapshot. Returns the changed count.
    pub fn update(&self, sparql: &str) -> Result<usize> {
        let mut db = self.writer.lock().unwrap();
        let result = db.query(sparql)?;
        let changed = match result {
            QueryResult::Update { changed } => changed,
            _ => {
                return Err(GStoreError::Query(
                    "ConcurrentDb::update expects a SPARQL UPDATE request".into(),
                ))
            }
        };
        self.publish(&db);
        Ok(changed)
    }

    /// Run a closure that mutates the database under the writer lock, then
    /// publish a new snapshot. Lets callers batch several mutations into one
    /// atomically-published version.
    pub fn write<F, T>(&self, f: F) -> T
    where
        F: FnOnce(&mut Database) -> T,
    {
        let mut db = self.writer.lock().unwrap();
        let out = f(&mut db);
        self.publish(&db);
        out
    }

    /// Publish a fresh snapshot from the writer database (bumping the version),
    /// returning the new version number.
    fn publish(&self, db: &Database) -> u64 {
        let next = self.version() + 1;
        let snap = Arc::new(Snapshot::from_db(db, next));
        *self.current.write().unwrap() = snap;
        next
    }

    // ---- optimistic multi-writer transactions -----------------------------

    /// Begin an optimistic transaction. Captures the current snapshot (version
    /// `V`) and buffers writes locally; nothing shared is mutated until
    /// [`Txn::commit`]. Many transactions can run concurrently.
    pub fn begin(&self) -> Txn<'_> {
        // Register liveness and capture the snapshot atomically under the OCC
        // lock, so a concurrent commit cannot GC a history entry we will need.
        let mut occ = self.occ.lock().unwrap();
        let snapshot = Arc::clone(&self.current.read().unwrap());
        let start_version = snapshot.version;
        occ.register(start_version);
        drop(occ);
        Txn {
            db: self,
            snapshot,
            start_version,
            ops: Vec::new(),
            keys: HashSet::new(),
            registered: true,
        }
    }

    /// Run `f` inside a fresh optimistic transaction, committing on return and
    /// retrying from a new snapshot on [`GStoreError::Conflict`], up to
    /// `max_attempts` times. Returns the closure's value on success, the last
    /// conflict (or a non-conflict error from `f`/commit) on failure.
    pub fn run_txn<F, T>(&self, max_attempts: usize, mut f: F) -> Result<T>
    where
        F: FnMut(&mut Txn<'_>) -> Result<T>,
    {
        let mut last = None;
        for _ in 0..max_attempts.max(1) {
            let mut txn = self.begin();
            let value = f(&mut txn)?;
            match txn.commit() {
                Ok(_version) => return Ok(value),
                Err(GStoreError::Conflict(msg)) => {
                    last = Some(GStoreError::Conflict(msg));
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Err(last
            .unwrap_or_else(|| GStoreError::Conflict("run_txn exhausted its retry budget".into())))
    }

    /// Number of retained history entries (used by tests to assert GC bounds).
    #[cfg(test)]
    fn history_len(&self) -> usize {
        self.occ.lock().unwrap().history.len()
    }
}

/// An optimistic transaction handle. Buffers writes against a captured snapshot
/// and applies them atomically on [`commit`](Txn::commit), or discards them if
/// dropped/aborted.
pub struct Txn<'a> {
    db: &'a ConcurrentDb,
    snapshot: Arc<Snapshot>,
    start_version: u64,
    ops: Vec<TxnOp>,
    keys: HashSet<TripleKey>,
    /// Whether this txn is still counted in `OccState::live_starts`.
    registered: bool,
}

impl Txn<'_> {
    /// The snapshot version this transaction reads.
    pub fn start_version(&self) -> u64 {
        self.start_version
    }

    /// The captured read snapshot (version `V`).
    pub fn snapshot(&self) -> &Arc<Snapshot> {
        &self.snapshot
    }

    /// Buffer an insert of `t` into the default graph.
    pub fn insert(&mut self, t: Triple) {
        self.keys.insert(t.to_string());
        self.ops.push(TxnOp::Insert(t));
    }

    /// Buffer a delete of `t` from the default graph.
    pub fn delete(&mut self, t: Triple) {
        self.keys.insert(t.to_string());
        self.ops.push(TxnOp::Delete(t));
    }

    /// Read query against the captured snapshot `V` (snapshot isolation). Does
    /// not reflect this txn's buffered writes — use [`contains`](Txn::contains)
    /// for read-your-writes point reads.
    pub fn query(&self, sparql: &str) -> Result<QueryResult> {
        self.snapshot.query(sparql)
    }

    /// Whether `t` is visible to this transaction: the snapshot's state with the
    /// txn's own buffered writes applied in order (read-your-writes).
    pub fn contains(&self, t: &Triple) -> bool {
        let key = t.to_string();
        let mut present = self.snapshot.contains(t);
        for op in &self.ops {
            match op {
                TxnOp::Insert(x) if x.to_string() == key => present = true,
                TxnOp::Delete(x) if x.to_string() == key => present = false,
                _ => {}
            }
        }
        present
    }

    /// Validate and commit. Serializes through the writer lock, so committers run
    /// one at a time (first-committer-wins). Returns the new committed version,
    /// or [`GStoreError::Conflict`] if a concurrent commit in `(V, current]`
    /// touched a triple key this txn also wrote.
    pub fn commit(mut self) -> Result<u64> {
        // Lock order is always writer → occ → current (see module docs).
        let mut db = self.db.writer.lock().unwrap();
        let mut occ = self.db.occ.lock().unwrap();

        // Validate against every commit newer than our start version.
        for (_v, keys) in occ.history.range((self.start_version + 1)..) {
            if !self.keys.is_disjoint(keys) {
                occ.deregister(self.start_version);
                occ.gc();
                self.registered = false;
                return Err(GStoreError::Conflict(format!(
                    "write-write conflict: a commit after version {} touched a key \
                     this transaction also wrote",
                    self.start_version
                )));
            }
        }

        // No conflict: apply the buffered writes to the authoritative database.
        for op in &self.ops {
            match op {
                TxnOp::Insert(t) => {
                    db.insert_triple(t);
                }
                TxnOp::Delete(t) => {
                    db.remove_triple(t);
                }
            }
        }
        let new_version = self.db.publish(&db);

        // Record this version's write set for future validators, then retire
        // ourselves from the live registry and GC unreachable history.
        let keys = std::mem::take(&mut self.keys);
        occ.history.insert(new_version, keys);
        occ.deregister(self.start_version);
        occ.gc();
        self.registered = false;
        Ok(new_version)
    }

    /// Abort explicitly, discarding all buffered writes. (Dropping a txn without
    /// committing has the same effect.)
    pub fn abort(self) {
        // `Drop` performs the deregistration + GC.
    }
}

impl Drop for Txn<'_> {
    fn drop(&mut self) {
        if self.registered {
            let mut occ = self.db.occ.lock().unwrap();
            occ.deregister(self.start_version);
            occ.gc();
            self.registered = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Term;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Barrier;
    use std::thread;

    const DATA: &str = "@prefix : <http://ex/> .\n:a :p :b .\n";

    fn cdb() -> ConcurrentDb {
        ConcurrentDb::build_from_str("conc", DATA).unwrap()
    }

    fn count(c: &ConcurrentDb, q: &str) -> usize {
        match c.query(q).unwrap() {
            QueryResult::Select(rs) => rs.row_count(),
            other => panic!("expected Select, got {other:?}"),
        }
    }

    fn snap_count(s: &Snapshot, q: &str) -> usize {
        match s.query(q).unwrap() {
            QueryResult::Select(rs) => rs.row_count(),
            other => panic!("expected Select, got {other:?}"),
        }
    }

    fn triple(s: &str, p: &str, o: &str) -> Triple {
        Triple::new(Term::iri(s), Term::iri(p), Term::iri(o))
    }

    #[test]
    fn single_thread_read_write_roundtrip() {
        let c = cdb();
        assert_eq!(c.version(), 1);
        assert_eq!(count(&c, "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }"), 1);

        let n = c
            .update("INSERT DATA { <http://ex/a> <http://ex/p> <http://ex/c> }")
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(c.version(), 2);
        assert_eq!(count(&c, "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }"), 2);

        c.update("DELETE DATA { <http://ex/a> <http://ex/p> <http://ex/c> }")
            .unwrap();
        assert_eq!(count(&c, "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }"), 1);
    }

    #[test]
    fn old_snapshot_is_stable_across_commits() {
        let c = cdb();
        let snap = c.snapshot(); // version 1: 1 triple
        c.update("INSERT DATA { <http://ex/a> <http://ex/p> <http://ex/c> }")
            .unwrap();
        // The held snapshot still sees the old state; the live db sees the new.
        match snap.query("SELECT ?o WHERE { ?s ?p ?o }").unwrap() {
            QueryResult::Select(rs) => assert_eq!(rs.row_count(), 1),
            other => panic!("{other:?}"),
        }
        assert_eq!(count(&c, "SELECT ?o WHERE { ?s ?p ?o }"), 2);
        assert_eq!(snap.version(), 1);
    }

    #[test]
    fn concurrent_readers_never_see_torn_state() {
        // Writer alternates the store between 1 and 2 triples; every concurrent
        // reader must observe exactly 1 or 2 — never a partial/torn count.
        let c = Arc::new(cdb());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let readers: Vec<_> = (0..4)
            .map(|_| {
                let c = Arc::clone(&c);
                let stop = Arc::clone(&stop);
                thread::spawn(move || {
                    let mut reads = 0u64;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let n = count(&c, "SELECT ?o WHERE { ?s ?p ?o }");
                        assert!(n == 1 || n == 2, "torn read: {n}");
                        reads += 1;
                    }
                    reads
                })
            })
            .collect();

        let writer = {
            let c = Arc::clone(&c);
            thread::spawn(move || {
                for _ in 0..200 {
                    c.update("INSERT DATA { <http://ex/a> <http://ex/p> <http://ex/c> }")
                        .unwrap();
                    c.update("DELETE DATA { <http://ex/a> <http://ex/p> <http://ex/c> }")
                        .unwrap();
                }
            })
        };

        writer.join().unwrap();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let total: u64 = readers.into_iter().map(|h| h.join().unwrap()).sum();
        assert!(total > 0, "readers should have run");
        // 400 commits happened on top of the initial version.
        assert!(c.version() >= 401, "version should advance per commit: {}", c.version());
    }

    // ---- optimistic multi-writer transactions ----------------------------

    #[test]
    fn txn_read_your_writes_and_snapshot_isolation() {
        let c = cdb();
        let mut txn = c.begin();
        let t = triple("http://ex/a", "http://ex/p", "http://ex/c");
        assert!(!txn.contains(&t)); // not yet
        txn.insert(t.clone());
        assert!(txn.contains(&t)); // read-your-writes
                                   // ...but the snapshot query (version V) does not see the buffered write.
        assert_eq!(snap_count(txn.snapshot(), "SELECT ?o WHERE { ?s ?p ?o }"), 1);
        let v = txn.commit().unwrap();
        assert_eq!(v, 2);
        assert_eq!(count(&c, "SELECT ?o WHERE { ?s ?p ?o }"), 2);
    }

    #[test]
    fn conflicting_txns_exactly_one_commits() {
        // Two threads both begin at version 1 and write the *same* triple key;
        // first-committer-wins means exactly one commits, the other conflicts.
        let c = Arc::new(cdb());
        let barrier = Arc::new(Barrier::new(2));
        let commits = Arc::new(AtomicUsize::new(0));
        let conflicts = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..2)
            .map(|_| {
                let c = Arc::clone(&c);
                let b = Arc::clone(&barrier);
                let cm = Arc::clone(&commits);
                let cf = Arc::clone(&conflicts);
                thread::spawn(move || {
                    let mut txn = c.begin();
                    txn.insert(triple("http://ex/x", "http://ex/p", "http://ex/y"));
                    // Ensure both transactions captured version 1 before either
                    // commits — otherwise the second would simply start later.
                    b.wait();
                    match txn.commit() {
                        Ok(_) => {
                            cm.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(GStoreError::Conflict(_)) => {
                            cf.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(e) => panic!("unexpected error: {e}"),
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(commits.load(Ordering::SeqCst), 1, "exactly one must commit");
        assert_eq!(conflicts.load(Ordering::SeqCst), 1, "the other must conflict");
        assert_eq!(c.version(), 2, "only one commit advanced the version");
    }

    #[test]
    fn run_txn_retries_conflicts_to_success() {
        // Many threads contend on one hot triple key via run_txn. Despite
        // conflicts, every logical transaction eventually commits.
        let c = Arc::new(cdb());
        let attempts = Arc::new(AtomicUsize::new(0));
        let n_threads = 4;
        let n_iters = 30;

        let handles: Vec<_> = (0..n_threads)
            .map(|_| {
                let c = Arc::clone(&c);
                let attempts = Arc::clone(&attempts);
                thread::spawn(move || {
                    for _ in 0..n_iters {
                        c.run_txn(10_000, |txn| {
                            attempts.fetch_add(1, Ordering::SeqCst);
                            txn.insert(triple("http://ex/hot", "http://ex/p", "http://ex/v"));
                            Ok(())
                        })
                        .expect("run_txn should retry conflicts to success");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        // No panics ⇒ all logical txns committed. The closure ran at least once
        // per logical txn; conflicts add extra attempts.
        let total_logical = n_threads * n_iters;
        assert!(
            attempts.load(Ordering::SeqCst) >= total_logical,
            "every logical transaction must run at least once"
        );
        assert_eq!(
            count(&c, "SELECT ?o WHERE { <http://ex/hot> <http://ex/p> ?o }"),
            1,
            "the hot triple must exist exactly once"
        );
    }

    #[test]
    fn disjoint_concurrent_txns_both_commit() {
        // Two threads begin at the same version but write disjoint triple keys —
        // no conflict, both commit.
        let c = Arc::new(cdb());
        let barrier = Arc::new(Barrier::new(2));

        let handles: Vec<_> = (0..2)
            .map(|i| {
                let c = Arc::clone(&c);
                let b = Arc::clone(&barrier);
                thread::spawn(move || {
                    let mut txn = c.begin();
                    let s = format!("http://ex/s{i}");
                    txn.insert(triple(&s, "http://ex/p", "http://ex/o"));
                    b.wait();
                    txn.commit()
                })
            })
            .collect();
        let results: Vec<Result<u64>> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        assert!(
            results.iter().all(|r| r.is_ok()),
            "disjoint txns must both commit: {results:?}"
        );
        assert_eq!(c.version(), 3, "two commits past the initial version");
        assert_eq!(
            count(&c, "SELECT ?s WHERE { ?s <http://ex/p> <http://ex/o> }"),
            2,
            "both disjoint triples must be present"
        );
    }

    #[test]
    fn reader_snapshot_unaffected_by_concurrent_commits() {
        // A long-lived read transaction keeps seeing version V even as writers
        // commit newer versions.
        let c = cdb();
        let reader = c.begin(); // captures version 1 (1 triple)
        assert_eq!(reader.start_version(), 1);

        // Commit several new versions via independent write transactions.
        for i in 0..5 {
            let mut w = c.begin();
            w.insert(triple(&format!("http://ex/n{i}"), "http://ex/p", "http://ex/o"));
            w.commit().unwrap();
        }

        // The reader's captured snapshot is unchanged...
        assert_eq!(snap_count(reader.snapshot(), "SELECT ?o WHERE { ?s ?p ?o }"), 1);
        match reader.query("SELECT ?o WHERE { ?s ?p ?o }").unwrap() {
            QueryResult::Select(rs) => assert_eq!(rs.row_count(), 1),
            other => panic!("{other:?}"),
        }
        // ...while the live database reflects every commit.
        assert_eq!(count(&c, "SELECT ?o WHERE { ?s ?p ?o }"), 6);
        assert_eq!(c.version(), 6);
    }

    #[test]
    fn history_gc_keeps_memory_bounded() {
        // With no long-lived transaction, the validation history is GC'd to
        // empty after every commit, so it never grows with the commit count.
        let c = cdb();
        for i in 0..400 {
            let s = format!("http://ex/g{i}");
            c.run_txn(4, |txn| {
                txn.insert(triple(&s, "http://ex/p", "http://ex/o"));
                Ok(())
            })
            .unwrap();
            assert!(
                c.history_len() <= 1,
                "history must stay bounded with no live txns, got {}",
                c.history_len()
            );
        }
        // Everything committed.
        assert_eq!(c.version(), 401);

        // A single long-lived txn pins history at its start; entries accumulate
        // only for versions after that start, and clear once it finishes.
        {
            let pin = c.begin();
            let start = pin.start_version();
            assert_eq!(start, 401);
            for i in 0..10 {
                let s = format!("http://ex/h{i}");
                c.run_txn(4, |txn| {
                    txn.insert(triple(&s, "http://ex/p", "http://ex/o"));
                    Ok(())
                })
                .unwrap();
            }
            // History retains exactly the 10 commits made after `start`.
            assert_eq!(c.history_len(), 10);
            drop(pin);
        }
        // Once the long-lived txn is gone, the next commit GCs everything again.
        c.run_txn(4, |txn| {
            txn.insert(triple("http://ex/z", "http://ex/p", "http://ex/o"));
            Ok(())
        })
        .unwrap();
        assert!(c.history_len() <= 1, "GC after the pin frees history");
    }
}
