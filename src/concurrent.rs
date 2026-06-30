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

