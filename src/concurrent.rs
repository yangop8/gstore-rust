//! Concurrent access with snapshot isolation — a scoped analogue of gStore's
//! `Txn_manager` + MVCC.
//!
//! [`ConcurrentDb`] lets many reader threads query while a single writer commits,
//! with **snapshot isolation**: a query runs against an immutable, consistent
//! [`Snapshot`] (an `Arc`), so readers never observe a half-applied write and
//! never block writers or each other. A writer serializes through a [`Mutex`],
//! mutates the authoritative [`Database`], then publishes a fresh snapshot by
//! atomically swapping the `Arc` — in-flight readers keep their old snapshot.
//!
//! Cost model: publishing a snapshot clones the dictionary + triple indexes
//! (`O(store)` per commit). That keeps published snapshots immutable and the
//! implementation simple; finer-grained MVCC (per-key version chains,
//! persistent/shared structures) would avoid the copy.
//!
//! NOT done (see `docs/REFACTOR_BACKLOG.md` E): per-key version chains, multiple
//! concurrent writers (writes are serialized), lock-free reads beyond the
//! `Arc`-swap, deadlock detection, and snapshot GC beyond `Arc` refcounting.

use std::sync::{Arc, Mutex, RwLock};

use std::collections::BTreeMap;

use crate::db::Database;
use crate::dict::Dictionary;
use crate::error::{GStoreError, Result};
use crate::parser::sparql;
use crate::query::{Evaluator, QueryResult};
use crate::store::TripleStore;

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

/// A thread-safe database wrapper offering concurrent snapshot-isolated reads and
/// serialized writes. Share it as `Arc<ConcurrentDb>` across threads.
pub struct ConcurrentDb {
    /// The authoritative mutable database; writers lock this (serialized).
    writer: Mutex<Database>,
    /// The currently-published immutable snapshot for readers.
    current: RwLock<Arc<Snapshot>>,
}

impl ConcurrentDb {
    /// Wrap an existing [`Database`].
    pub fn new(db: Database) -> ConcurrentDb {
        let snap = Arc::new(Snapshot::from_db(&db, 1));
        ConcurrentDb {
            writer: Mutex::new(db),
            current: RwLock::new(snap),
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

    /// Publish a fresh snapshot from the writer database (bumping the version).
    fn publish(&self, db: &Database) {
        let next = self.version() + 1;
        let snap = Arc::new(Snapshot::from_db(db, next));
        *self.current.write().unwrap() = snap;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
