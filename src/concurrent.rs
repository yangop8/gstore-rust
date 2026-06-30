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
//! Cost model: publishing a [`Snapshot`] clones the dictionary + triple indexes
//! (`O(store)` per commit). That keeps published snapshots immutable and the
//! `query()`/`snapshot()` read path simple.
//!
//! On top of that clone-published snapshot, a **per-key version-chain** store
//! provides finer-grained MVCC *without* a per-snapshot copy. Every transactional
//! commit appends, for each written triple key, a version record — an `insert` or
//! a `tombstone` — stamped with the commit version. A version-pinned reader
//! ([`ConcurrentDb::version_view`] → [`VersionView`]) resolves each key to the
//! latest record `≤` its pinned version `V`, falling back to a single shared,
//! immutable base snapshot (captured at construction) for keys it never touched.
//! So many version-pinned readers *share* one chain structure and merely filter
//! by version, rather than each holding a full clone: point reads
//! ([`VersionView::contains`]) clone nothing, and [`VersionView::query`]
//! materializes the visible state only on demand. Records below the oldest live
//! reader's version are garbage-collected, so each chain collapses to one record
//! per key in steady state.
//!
//! Scope note: the legacy `update()`/`write()` path stays snapshot-consistent for
//! readers and records its commit version, so a [`Txn`] whose lifetime overlaps a
//! legacy write conservatively conflicts on commit (it cannot lose that write
//! silently). It is conservative — a legacy write conflicts ALL transactions live
//! at the time, regardless of key overlap, because that path does not record a
//! per-key write set. The version-chain view likewise reflects the construction
//! base plus all committed [`Txn`]s but not legacy writes, so prefer the
//! transactional path when reading through [`VersionView`].
//!
//! Isolation levels: [`begin_with`](ConcurrentDb::begin_with) selects an
//! [`IsolationLevel`] (gStore's `Txn_manager` `IsolationLevelType`) —
//! `Snapshot` (the default, write-write checks), `Serializable` (write-write +
//! read-set / read-write checks, preventing write skew), or `ReadCommitted`
//! (latest-committed reads, no commit-time conflict abort).
//!
//! Reader/writer concurrency (backlog item 7): readers already run fully
//! concurrently with writers — a reader takes the latest [`Snapshot`] via the
//! `current` `RwLock` (a brief read-lock, released before evaluation) or pins a
//! [`VersionView`], and never contends on the writer `Mutex`. The writer path
//! stays a `Mutex` because committing is intrinsically exclusive (validate →
//! apply → publish must be atomic); a `RwLock` there would add no reader
//! concurrency (readers don't use it) and could not admit two concurrent
//! writers without breaking first-committer-wins. So no change is made to the
//! writer lock; finer-grained per-key write latching is left to a future MVCC
//! pass.
//!
//! NOT done (see `docs/REFACTOR_BACKLOG.md` E): lock-free reads beyond the
//! `Arc`-swap / `RwLock`, deadlock detection, and version-chaining the legacy
//! `update()`/`write()` path.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};

use crate::db::Database;
use crate::dict::Dictionary;
use crate::error::{GStoreError, Result};
use crate::model::{IdTriple, Triple};
use crate::parser::sparql;
use crate::query::{Evaluator, QueryResult};
use crate::store::TripleStore;

/// A canonical, dictionary-independent key for a triple: its N-Triples surface
/// form (which round-trips losslessly), used for write-write conflict detection.
type TripleKey = String;

/// Transaction isolation level, selectable at [`ConcurrentDb::begin_with`]
/// (gStore's `Txn_manager` `IsolationLevelType`). The levels differ in their
/// commit-time validation and read visibility:
///
/// | level | reads | commit aborts on |
/// |-------|-------|------------------|
/// | [`ReadCommitted`](Self::ReadCommitted) | latest committed (re-read each time) | nothing (last-writer-wins) |
/// | [`Snapshot`](Self::Snapshot) | the snapshot captured at `begin` | a concurrent commit that **wrote** a key this txn also wrote (write-write) |
/// | [`Serializable`](Self::Serializable) | the snapshot captured at `begin` | write-write **or** a concurrent commit that wrote a key this txn **read** (read-write) |
///
/// So `Serializable` is strictly stronger than `Snapshot` (it additionally
/// prevents write-skew via read-set validation), and `ReadCommitted` is the
/// weakest (no snapshot stability, no write-write abort).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IsolationLevel {
    /// Reads observe the latest committed state; commit never aborts on conflict.
    ReadCommitted,
    /// Snapshot isolation: stable snapshot reads + write-write conflict checks.
    #[default]
    Snapshot,
    /// Serializable: snapshot reads + write-write **and** read-write checks.
    Serializable,
}

impl IsolationLevel {
    /// Whether commit performs write-write conflict validation.
    fn checks_write_write(self) -> bool {
        matches!(self, IsolationLevel::Snapshot | IsolationLevel::Serializable)
    }
    /// Whether commit performs read-write conflict validation (anti-write-skew).
    fn checks_read_write(self) -> bool {
        matches!(self, IsolationLevel::Serializable)
    }
}

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
    /// Version of the most recent *legacy* (non-transactional) `update()`/`write()`
    /// commit. Those paths don't record a per-key write set, so a `Txn` whose
    /// lifetime overlaps one conservatively conflicts (no silent lost update).
    last_legacy_version: u64,
}

impl OccState {
    fn new() -> OccState {
        OccState {
            history: BTreeMap::new(),
            live_starts: BTreeMap::new(),
            last_legacy_version: 0,
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

    /// The GC floor: the smallest live start/pin version. History entries at or
    /// below it are unreachable, and version records strictly below each key's
    /// anchor at it can be reclaimed. `u64::MAX` when nothing is live.
    fn gc_floor(&self) -> u64 {
        self.live_starts.keys().next().copied().unwrap_or(u64::MAX)
    }

    /// Drop history entries no live transaction can still need. A txn that
    /// started at `s` only validates versions `> s`, so anything at or below the
    /// smallest live start is unreachable; with no live txns, all of it is.
    fn gc(&mut self) {
        let floor = self.gc_floor();
        self.history.retain(|&v, _| v > floor);
    }
}

/// Whether a version record adds a triple (`Insert`) or removes it (`Tombstone`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChainOp {
    Insert,
    Tombstone,
}

/// One entry in a triple key's version chain: the committing transaction's
/// version and whether that commit left the triple present or absent.
#[derive(Debug, Clone, Copy)]
struct VersionRecord {
    version: u64,
    op: ChainOp,
}

/// The version chain for a single triple key. The triple is held once (every
/// record in the chain shares it); `records` are ordered by ascending version —
/// commit versions increase monotonically, so appends stay sorted.
#[derive(Debug)]
struct Chain {
    triple: Triple,
    records: Vec<VersionRecord>,
}

impl Chain {
    /// The record visible to a reader at `version`: the latest record with
    /// `record.version <= version`, or `None` if every record is newer.
    fn visible(&self, version: u64) -> Option<&VersionRecord> {
        self.records.iter().rev().find(|r| r.version <= version)
    }
}

/// A per-key version-chain store shared (behind an `RwLock`) by every
/// version-pinned reader. Maps each triple key to its [`Chain`]; readers filter
/// by version instead of cloning the store.
#[derive(Debug, Default)]
struct VersionedStore {
    chains: HashMap<TripleKey, Chain>,
}

impl VersionedStore {
    fn new() -> VersionedStore {
        VersionedStore::default()
    }

    /// Append a version record for `triple`'s key at `version`.
    fn record(&mut self, version: u64, triple: Triple, op: ChainOp) {
        let key = triple.to_string();
        self.chains
            .entry(key)
            .or_insert_with(|| Chain {
                triple,
                records: Vec::new(),
            })
            .records
            .push(VersionRecord { version, op });
    }

    /// Whether `key` is present at `version`: `Some(true/false)` from the latest
    /// chain record `<= version`, or `None` when the chain has no such record (the
    /// caller should then consult the base snapshot).
    fn visible_present(&self, key: &str, version: u64) -> Option<bool> {
        self.chains
            .get(key)
            .and_then(|c| c.visible(version))
            .map(|r| r.op == ChainOp::Insert)
    }

    /// Apply every chain's visible state at `version` onto a base `dict`/`store`
    /// (clones of the base snapshot): insert visible triples, remove tombstoned
    /// ones. Keys with no record `<= version` are left to the base untouched.
    fn materialize_into(&self, version: u64, dict: &mut Dictionary, store: &mut TripleStore) {
        for chain in self.chains.values() {
            let Some(rec) = chain.visible(version) else {
                continue;
            };
            let t = &chain.triple;
            match rec.op {
                ChainOp::Insert => {
                    let id = IdTriple::new(
                        dict.intern_entity(&t.subject.dict_key()),
                        dict.intern_predicate(&t.predicate.dict_key()),
                        dict.intern_term(&t.object),
                    );
                    store.insert(id);
                }
                ChainOp::Tombstone => {
                    if let (Some(s), Some(p), Some(o)) = (
                        dict.entity_id(&t.subject.dict_key()),
                        dict.predicate_id(&t.predicate.dict_key()),
                        dict.term_id(&t.object),
                    ) {
                        store.remove(IdTriple::new(s, p, o));
                    }
                }
            }
        }
    }

    /// Reclaim records no live reader can observe: for each chain keep the latest
    /// record `<= floor` (the oldest live reader's anchor) plus everything newer,
    /// dropping strictly-older records. With no live readers (`floor == u64::MAX`)
    /// every chain collapses to its single latest record.
    fn gc(&mut self, floor: u64) {
        for chain in self.chains.values_mut() {
            if let Some(anchor) = chain.records.iter().rposition(|r| r.version <= floor) {
                chain.records.drain(0..anchor);
            }
        }
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
    /// Immutable base snapshot captured at construction. Version-pinned readers
    /// ([`VersionView`]) fall back to it for keys no transaction has touched, so
    /// they need not clone the store per snapshot.
    base: Arc<Snapshot>,
    /// Per-key version chains backing [`VersionView`] reads.
    versioned: RwLock<VersionedStore>,
}

impl ConcurrentDb {
    /// Wrap an existing [`Database`].
    pub fn new(db: Database) -> ConcurrentDb {
        let snap = Arc::new(Snapshot::from_db(&db, 1));
        ConcurrentDb {
            base: Arc::clone(&snap),
            writer: Mutex::new(db),
            current: RwLock::new(snap),
            occ: Mutex::new(OccState::new()),
            versioned: RwLock::new(VersionedStore::new()),
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
        // Record the legacy-write version under the OCC lock so overlapping
        // transactions detect it (writer → occ → current order preserved).
        let mut occ = self.occ.lock().unwrap();
        let v = self.publish(&db);
        occ.last_legacy_version = v;
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
        let mut occ = self.occ.lock().unwrap();
        let v = self.publish(&db);
        occ.last_legacy_version = v;
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

    /// Begin an optimistic transaction at the default [`IsolationLevel::Snapshot`].
    /// Captures the current snapshot (version `V`) and buffers writes locally;
    /// nothing shared is mutated until [`Txn::commit`]. Many transactions can run
    /// concurrently.
    pub fn begin(&self) -> Txn<'_> {
        self.begin_with(IsolationLevel::Snapshot)
    }

    /// Begin an optimistic transaction at a chosen [`IsolationLevel`] (gStore's
    /// `Txn_manager::Begin(IsolationLevelType)`). The level governs read
    /// visibility and the commit-time conflict checks — see [`IsolationLevel`].
    pub fn begin_with(&self, level: IsolationLevel) -> Txn<'_> {
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
            reads: HashSet::new(),
            level,
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

    // ---- per-key version-chain MVCC reads ---------------------------------

    /// Open a version-pinned reader at the latest committed version. The reader
    /// shares the per-key version chains (no per-snapshot clone) and resolves
    /// each key to the version visible at its pin; concurrent commits never
    /// change what it sees. Registers liveness so GC keeps the versions it needs
    /// until it is dropped.
    pub fn version_view(&self) -> VersionView<'_> {
        // Capture the version + register liveness atomically under the OCC lock,
        // so a concurrent commit cannot GC a version this view will need.
        let mut occ = self.occ.lock().unwrap();
        let version = self.current.read().unwrap().version;
        occ.register(version);
        drop(occ);
        VersionView {
            db: self,
            version,
            registered: true,
        }
    }

    /// Open a version-pinned reader at a specific committed `version`
    /// (time-travel). Like [`version_view`](Self::version_view) it registers
    /// liveness for GC. `version` should be a committed version `<=` the current
    /// one; reads resolve through the version chains as of that version.
    pub fn version_view_at(&self, version: u64) -> VersionView<'_> {
        let mut occ = self.occ.lock().unwrap();
        occ.register(version);
        drop(occ);
        VersionView {
            db: self,
            version,
            registered: true,
        }
    }

    /// Garbage-collect the version chains down to `floor` (the oldest live
    /// snapshot version). Callers compute `floor` from [`OccState::gc_floor`]
    /// while holding the OCC lock, preserving the writer → occ → versioned order.
    fn gc_versions(&self, floor: u64) {
        self.versioned.write().unwrap().gc(floor);
    }

    /// Number of retained history entries (used by tests to assert GC bounds).
    #[cfg(test)]
    fn history_len(&self) -> usize {
        self.occ.lock().unwrap().history.len()
    }

    /// Number of version records retained in a triple key's chain (tests).
    #[cfg(test)]
    fn chain_len(&self, key: &str) -> usize {
        self.versioned
            .read()
            .unwrap()
            .chains
            .get(key)
            .map_or(0, |c| c.records.len())
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
    /// Triple keys this transaction has *read* (via [`Txn::read`]). Used only by
    /// [`IsolationLevel::Serializable`] for read-write conflict validation.
    reads: HashSet<TripleKey>,
    /// This transaction's isolation level.
    level: IsolationLevel,
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

    /// This transaction's isolation level.
    pub fn isolation(&self) -> IsolationLevel {
        self.level
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

    /// Read query. Under [`Snapshot`](IsolationLevel::Snapshot)/
    /// [`Serializable`](IsolationLevel::Serializable) it runs against the captured
    /// snapshot `V` (snapshot isolation); under
    /// [`ReadCommitted`](IsolationLevel::ReadCommitted) it runs against the latest
    /// committed snapshot (so it observes concurrent commits). It does not reflect
    /// this txn's own buffered writes — use [`contains`](Txn::contains) for
    /// read-your-writes point reads.
    pub fn query(&self, sparql: &str) -> Result<QueryResult> {
        match self.level {
            IsolationLevel::ReadCommitted => self.db.snapshot().query(sparql),
            _ => self.snapshot.query(sparql),
        }
    }

    /// Whether `t` is visible to this transaction: the base state (snapshot `V`
    /// for snapshot/serializable, or the latest committed snapshot for read
    /// committed) with the txn's own buffered writes applied in order
    /// (read-your-writes).
    pub fn contains(&self, t: &Triple) -> bool {
        let key = t.to_string();
        let mut present = match self.level {
            IsolationLevel::ReadCommitted => self.db.snapshot().contains(t),
            _ => self.snapshot.contains(t),
        };
        for op in &self.ops {
            match op {
                TxnOp::Insert(x) if x.to_string() == key => present = true,
                TxnOp::Delete(x) if x.to_string() == key => present = false,
                _ => {}
            }
        }
        present
    }

    /// A *tracked* point read: like [`contains`](Txn::contains) but also records
    /// `t`'s key in this transaction's read set, so a
    /// [`Serializable`](IsolationLevel::Serializable) commit will abort if a
    /// concurrent commit later wrote that key (read-write conflict / anti-write-
    /// skew). For other levels the read set is ignored.
    pub fn read(&mut self, t: &Triple) -> bool {
        self.reads.insert(t.to_string());
        self.contains(t)
    }

    /// Validate and commit. Serializes through the writer lock, so committers run
    /// one at a time (first-committer-wins). Returns the new committed version,
    /// or [`GStoreError::Conflict`] if a concurrent commit in `(V, current]`
    /// touched a triple key this txn also wrote.
    pub fn commit(mut self) -> Result<u64> {
        // Lock order is always writer → occ → current (see module docs).
        let mut db = self.db.writer.lock().unwrap();
        let mut occ = self.db.occ.lock().unwrap();

        // Conflict checks depend on the isolation level (see `IsolationLevel`).
        let ww = self.level.checks_write_write();
        let rw = self.level.checks_read_write();
        // A legacy (non-transactional) write during our lifetime is not key-
        // tracked, so snapshot/serializable conservatively conflict to avoid a
        // silent lost update; read-committed tolerates it.
        let legacy_during = ww && occ.last_legacy_version > self.start_version;
        // Write-write: a newer commit wrote a key this txn also wrote.
        let ww_conflict = ww
            && !legacy_during
            && occ
                .history
                .range((self.start_version + 1)..)
                .any(|(_v, keys)| !self.keys.is_disjoint(keys));
        // Read-write (serializable only): a newer commit wrote a key this txn
        // read — would break serializability (write skew).
        let rw_conflict = rw
            && !legacy_during
            && !ww_conflict
            && occ
                .history
                .range((self.start_version + 1)..)
                .any(|(_v, keys)| !self.reads.is_disjoint(keys));
        if legacy_during || ww_conflict || rw_conflict {
            occ.deregister(self.start_version);
            occ.gc();
            self.db.gc_versions(occ.gc_floor());
            self.registered = false;
            let kind = if legacy_during {
                "non-transactional write"
            } else if rw_conflict {
                "committed transaction wrote a key this transaction read"
            } else {
                "committed transaction"
            };
            return Err(GStoreError::Conflict(format!(
                "conflict: a {} after version {} may overlap this transaction",
                kind, self.start_version
            )));
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

        // Append this commit's per-key version records to the shared chains: one
        // record per written key, reflecting that key's net final state under
        // this transaction (a later op for the same key supersedes an earlier).
        let mut net: BTreeMap<TripleKey, (Triple, ChainOp)> = BTreeMap::new();
        for op in &self.ops {
            match op {
                TxnOp::Insert(t) => {
                    net.insert(t.to_string(), (t.clone(), ChainOp::Insert));
                }
                TxnOp::Delete(t) => {
                    net.insert(t.to_string(), (t.clone(), ChainOp::Tombstone));
                }
            }
        }
        {
            let mut vs = self.db.versioned.write().unwrap();
            for (_, (triple, op)) in net {
                vs.record(new_version, triple, op);
            }
        }

        // Record this version's write set for future validators, then retire
        // ourselves from the live registry and GC unreachable history + versions.
        let keys = std::mem::take(&mut self.keys);
        occ.history.insert(new_version, keys);
        occ.deregister(self.start_version);
        occ.gc();
        self.db.gc_versions(occ.gc_floor());
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
            self.db.gc_versions(occ.gc_floor());
            self.registered = false;
        }
    }
}

/// A version-pinned reader over the per-key version chains. Holding one is cheap
/// — a borrow plus a version number — so concurrent readers *share* the chain
/// structure instead of each cloning the store. It resolves each triple key to
/// the version visible at [`version`](Self::version): the latest chain record
/// `<= version` (insert ⇒ present, tombstone ⇒ absent), falling back to the
/// shared immutable base snapshot for keys no transaction has touched. Dropping
/// it releases its GC pin.
pub struct VersionView<'a> {
    db: &'a ConcurrentDb,
    version: u64,
    /// Whether this view is still counted in `OccState::live_starts`.
    registered: bool,
}

impl VersionView<'_> {
    /// The version this view is pinned at.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Whether `t` is visible at this view's version. Resolves through the shared
    /// version chains (latest record `<= version`) and only consults the base
    /// snapshot for keys with no such record — cloning nothing.
    pub fn contains(&self, t: &Triple) -> bool {
        let key = t.to_string();
        match self
            .db
            .versioned
            .read()
            .unwrap()
            .visible_present(&key, self.version)
        {
            Some(present) => present,
            None => self.db.base.contains(t),
        }
    }

    /// Run a read query as of this view's version. Materializes the visible
    /// default-graph state on demand — the base snapshot plus chain overrides
    /// `<= version` — then evaluates against it.
    pub fn query(&self, sparql: &str) -> Result<QueryResult> {
        let mut dict = self.db.base.dict.clone();
        let mut store = self.db.base.store.clone();
        self.db
            .versioned
            .read()
            .unwrap()
            .materialize_into(self.version, &mut dict, &mut store);
        let q = sparql::parse(sparql)?;
        let mut eval = Evaluator::new(&dict, &store);
        if !self.db.base.named.is_empty() {
            eval = eval.with_named(&self.db.base.named);
        }
        eval.evaluate(&q)
    }
}

impl Drop for VersionView<'_> {
    fn drop(&mut self) {
        if self.registered {
            let mut occ = self.db.occ.lock().unwrap();
            occ.deregister(self.version);
            occ.gc();
            self.db.gc_versions(occ.gc_floor());
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

    fn view_count(v: &VersionView, q: &str) -> usize {
        match v.query(q).unwrap() {
            QueryResult::Select(rs) => rs.row_count(),
            other => panic!("expected Select, got {other:?}"),
        }
    }

    fn triple(s: &str, p: &str, o: &str) -> Triple {
        Triple::new(Term::iri(s), Term::iri(p), Term::iri(o))
    }

    #[test]
    fn legacy_write_during_txn_conflicts() {
        let c = cdb();
        let mut txn = c.begin(); // starts at version 1
        // A legacy (non-transactional) update commits during the txn's lifetime.
        c.update("INSERT DATA { <http://ex/a> <http://ex/p> <http://ex/x> }")
            .unwrap();
        txn.insert(triple("http://ex/q", "http://ex/p", "http://ex/r"));
        let res = txn.commit();
        assert!(
            matches!(res, Err(GStoreError::Conflict(_))),
            "a legacy write during the txn must conflict, got {res:?}"
        );
        // The legacy write persisted; the aborted txn's buffered write did not.
        assert_eq!(count(&c, "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }"), 2);
        assert_eq!(count(&c, "SELECT ?s WHERE { ?s <http://ex/p> <http://ex/r> }"), 0);
    }

    #[test]
    fn legacy_write_before_txn_start_does_not_conflict() {
        let c = cdb();
        c.update("INSERT DATA { <http://ex/a> <http://ex/p> <http://ex/x> }")
            .unwrap(); // version 2
        let mut txn = c.begin(); // starts at version 2, after the legacy write
        txn.insert(triple("http://ex/q", "http://ex/p", "http://ex/r"));
        assert!(
            txn.commit().is_ok(),
            "a legacy write before the txn started must not conflict"
        );
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

    // ---- per-key version-chain MVCC --------------------------------------

    #[test]
    fn version_view_pinned_across_commits() {
        // A long-running version-pinned reader keeps seeing version V even as
        // transactions commit newer versions onto the shared chains.
        let c = cdb();
        let base = triple("http://ex/a", "http://ex/p", "http://ex/b");
        let view = c.version_view(); // pins version 1 (just the base triple)
        assert_eq!(view.version(), 1);
        assert!(view.contains(&base));

        for i in 0..5 {
            let mut w = c.begin();
            w.insert(triple(&format!("http://ex/n{i}"), "http://ex/p", "http://ex/o"));
            w.commit().unwrap();
        }

        // The pin still resolves to the base state, despite five newer commits.
        assert!(view.contains(&base));
        assert!(!view.contains(&triple("http://ex/n0", "http://ex/p", "http://ex/o")));
        assert_eq!(view.version(), 1);

        // A fresh view sees every committed version.
        let latest = c.version_view();
        assert_eq!(latest.version(), 6);
        assert!(latest.contains(&triple("http://ex/n4", "http://ex/p", "http://ex/o")));
    }

    #[test]
    fn version_chain_visible_version_selection() {
        // One key carries three versions; each pinned version resolves to the
        // record visible at it (latest <= version).
        let c = cdb();
        let k = triple("http://ex/k", "http://ex/p", "http://ex/v");
        // Pin the base version so GC retains the whole chain while we inspect it.
        let pin = c.version_view_at(1);

        c.run_txn(4, |t| {
            t.insert(k.clone());
            Ok(())
        })
        .unwrap(); // v2: insert
        c.run_txn(4, |t| {
            t.delete(k.clone());
            Ok(())
        })
        .unwrap(); // v3: tombstone
        c.run_txn(4, |t| {
            t.insert(k.clone());
            Ok(())
        })
        .unwrap(); // v4: insert again

        assert_eq!(c.version(), 4);
        assert_eq!(c.chain_len(&k.to_string()), 3, "all three versions retained");

        assert!(!c.version_view_at(1).contains(&k), "v1: before any record");
        assert!(c.version_view_at(2).contains(&k), "v2: inserted");
        assert!(!c.version_view_at(3).contains(&k), "v3: tombstoned");
        assert!(c.version_view_at(4).contains(&k), "v4: re-inserted");

        drop(pin);
    }

    #[test]
    fn version_chain_tombstone_visibility() {
        // A tombstone masks a base triple for readers at/after the deleting
        // version; earlier pins still see the base value.
        let c = cdb();
        let base = triple("http://ex/a", "http://ex/p", "http://ex/b");
        let pin = c.version_view_at(1); // retain the chain for inspection

        c.run_txn(4, |t| {
            t.delete(base.clone());
            Ok(())
        })
        .unwrap(); // v2: tombstone the base triple
        c.run_txn(4, |t| {
            t.insert(base.clone());
            Ok(())
        })
        .unwrap(); // v3: re-insert it

        assert_eq!(c.chain_len(&base.to_string()), 2);
        assert!(c.version_view_at(1).contains(&base), "v1: base present");
        assert!(!c.version_view_at(2).contains(&base), "v2: tombstoned");
        assert!(c.version_view_at(3).contains(&base), "v3: re-inserted");

        drop(pin);
    }

    #[test]
    fn write_write_conflict_on_same_key() {
        // Two transactions begin at the same version and write the same key:
        // first-committer-wins, and exactly one version record is appended.
        let c = cdb();
        let k = triple("http://ex/x", "http://ex/p", "http://ex/y");
        let mut a = c.begin(); // version 1
        let mut b = c.begin(); // version 1
        a.insert(k.clone());
        b.insert(k.clone());

        assert!(a.commit().is_ok(), "first committer wins");
        let res = b.commit();
        assert!(
            matches!(res, Err(GStoreError::Conflict(_))),
            "the second writer of the same key must conflict, got {res:?}"
        );
        assert_eq!(c.version(), 2, "only one commit advanced the version");
        assert_eq!(
            c.chain_len(&k.to_string()),
            1,
            "only the winner appended a version record"
        );
    }

    #[test]
    fn gc_reclaims_only_below_oldest_live_view() {
        // While an old view pins the floor, every version at/after it is kept;
        // once the pin drops, the chain collapses to the single latest record.
        let c = cdb();
        let k = triple("http://ex/k", "http://ex/p", "http://ex/v");
        let pin = c.version_view_at(1);

        c.run_txn(4, |t| {
            t.insert(k.clone());
            Ok(())
        })
        .unwrap(); // v2
        c.run_txn(4, |t| {
            t.delete(k.clone());
            Ok(())
        })
        .unwrap(); // v3
        c.run_txn(4, |t| {
            t.insert(k.clone());
            Ok(())
        })
        .unwrap(); // v4

        // Floor pinned at 1 ⇒ nothing below the v1 anchor is reclaimed.
        assert_eq!(c.chain_len(&k.to_string()), 3);

        // Dropping the pin lowers the floor; the GC run in `drop` collapses the
        // chain to its latest record (insert at v4).
        drop(pin);
        assert_eq!(c.chain_len(&k.to_string()), 1);
        assert!(c.version_view().contains(&k), "latest record is the insert");
    }

    #[test]
    fn version_view_query_reflects_pinned_version() {
        // SPARQL through a version view materializes the visible state at its pin.
        let c = cdb();
        let pin = c.version_view_at(1);
        c.run_txn(4, |t| {
            t.insert(triple("http://ex/a", "http://ex/p", "http://ex/c"));
            Ok(())
        })
        .unwrap(); // v2 adds a second object for <a> <p>

        let v1 = c.version_view_at(1);
        let v2 = c.version_view_at(2);
        assert_eq!(
            view_count(&v1, "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }"),
            1,
            "v1 sees only the base object"
        );
        assert_eq!(
            view_count(&v2, "SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }"),
            2,
            "v2 sees the base plus the committed object"
        );
        drop(pin);
    }

    #[test]
    fn version_view_stable_under_concurrent_commits() {
        // A pinned view, held while writer threads commit concurrently, still
        // resolves to exactly its pinned version afterwards. The assertions run
        // after the threads join, so the test is deterministic.
        let c = cdb();
        let base = triple("http://ex/a", "http://ex/p", "http://ex/b");
        let view = c.version_view(); // pins version 1

        thread::scope(|s| {
            for _ in 0..2 {
                s.spawn(|| {
                    for i in 0..10 {
                        c.run_txn(10_000, |t| {
                            t.insert(triple(
                                &format!("http://ex/c{i}"),
                                "http://ex/p",
                                "http://ex/o",
                            ));
                            Ok(())
                        })
                        .unwrap();
                    }
                });
            }
        });

        // Despite the concurrent commits, the pin sees only the base.
        assert!(view.contains(&base));
        assert!(!view.contains(&triple("http://ex/c0", "http://ex/p", "http://ex/o")));
        assert_eq!(view.version(), 1);
        // Both threads contended on the same keys; conflicts were retried to
        // success via run_txn, so every commit advanced the shared version.
        assert!(c.version() >= 11, "concurrent commits advanced the version");
    }

    // ---- isolation levels ------------------------------------------------

    fn txn_count(t: &Txn, q: &str) -> usize {
        match t.query(q).unwrap() {
            QueryResult::Select(rs) => rs.row_count(),
            other => panic!("expected Select, got {other:?}"),
        }
    }

    #[test]
    fn begin_defaults_to_snapshot_isolation() {
        let c = cdb();
        assert_eq!(c.begin().isolation(), IsolationLevel::Snapshot);
        assert_eq!(
            c.begin_with(IsolationLevel::Serializable).isolation(),
            IsolationLevel::Serializable
        );
    }

    /// A txn that *reads* key K1 and *writes* a disjoint key K2, while a
    /// concurrent committer writes K1. Snapshot isolation does not check read sets
    /// (so it commits — a write-skew); Serializable does (so it aborts).
    fn read_write_skew(level: IsolationLevel) -> Result<u64> {
        let c = cdb();
        let mut rw = c.begin_with(level); // version 1
        let k1 = triple("http://ex/x", "http://ex/p", "http://ex/y");
        let _ = rw.read(&k1); // record a read of K1

        // A concurrent transaction writes K1 and commits.
        let mut w = c.begin();
        w.insert(k1.clone());
        w.commit().unwrap();

        // RW now writes a disjoint key and tries to commit.
        rw.insert(triple("http://ex/u", "http://ex/p", "http://ex/v"));
        rw.commit()
    }

    #[test]
    fn snapshot_permits_write_skew() {
        assert!(
            read_write_skew(IsolationLevel::Snapshot).is_ok(),
            "snapshot isolation does not validate read sets"
        );
    }

    #[test]
    fn serializable_detects_read_write_conflict() {
        let res = read_write_skew(IsolationLevel::Serializable);
        assert!(
            matches!(res, Err(GStoreError::Conflict(_))),
            "serializable must abort on a read-write conflict, got {res:?}"
        );
    }

    #[test]
    fn serializable_still_commits_without_read_conflict() {
        // Serializable must not abort spuriously: if nothing it read was written
        // by a concurrent commit, it commits.
        let c = cdb();
        let mut rw = c.begin_with(IsolationLevel::Serializable);
        let _ = rw.read(&triple("http://ex/x", "http://ex/p", "http://ex/y"));
        // Concurrent commit writes a *different* key than RW read or writes.
        let mut w = c.begin();
        w.insert(triple("http://ex/m", "http://ex/p", "http://ex/n"));
        w.commit().unwrap();
        rw.insert(triple("http://ex/u", "http://ex/p", "http://ex/v"));
        assert!(rw.commit().is_ok());
    }

    #[test]
    fn read_committed_tolerates_write_write_conflict() {
        // Two read-committed txns write the same key; both commit (no abort),
        // unlike snapshot isolation where the second would conflict.
        let c = cdb();
        let k = triple("http://ex/x", "http://ex/p", "http://ex/y");
        let mut a = c.begin_with(IsolationLevel::ReadCommitted);
        let mut b = c.begin_with(IsolationLevel::ReadCommitted);
        a.insert(k.clone());
        b.insert(k.clone());
        assert!(a.commit().is_ok());
        assert!(
            b.commit().is_ok(),
            "read-committed does not abort on write-write"
        );
        assert_eq!(c.version(), 3, "both commits advanced the version");
    }

    #[test]
    fn read_committed_sees_concurrent_commits_but_snapshot_does_not() {
        let c = cdb();
        let rc = c.begin_with(IsolationLevel::ReadCommitted); // version 1
        let snap = c.begin_with(IsolationLevel::Snapshot); // version 1

        // A concurrent transaction commits a new triple.
        let mut w = c.begin();
        w.insert(triple("http://ex/a", "http://ex/p", "http://ex/c"));
        w.commit().unwrap();

        let q = "SELECT ?o WHERE { ?s ?p ?o }";
        assert_eq!(
            txn_count(&rc, q),
            2,
            "read-committed observes the concurrent commit"
        );
        assert_eq!(
            txn_count(&snap, q),
            1,
            "snapshot isolation keeps its captured view"
        );
    }

    // ---- redo log over committed transactions ----------------------------

    #[test]
    fn redo_log_captures_committed_transactions() {
        let dir = std::env::temp_dir().join("gstore_conc_redo_ut");
        let _ = std::fs::remove_dir_all(&dir);
        let c = cdb();
        // Enable the redo log on the wrapped database.
        c.write(|db| db.enable_redo_log_in(&dir)).unwrap();

        // Two committed transactions: one insert, one delete of the base triple.
        c.run_txn(4, |t| {
            t.insert(triple("http://ex/k", "http://ex/p", "http://ex/v"));
            Ok(())
        })
        .unwrap();
        c.run_txn(4, |t| {
            t.delete(triple("http://ex/a", "http://ex/p", "http://ex/b"));
            Ok(())
        })
        .unwrap();

        // Recover into a fresh database from the same base document.
        let mut recovered = Database::build_from_str("conc", DATA).unwrap();
        let applied = recovered.replay_redo_log(dir.join("redo.log")).unwrap();
        assert_eq!(applied, 2, "both committed transactions were logged");
        assert_eq!(
            recovered
                .select("SELECT ?o WHERE { <http://ex/k> <http://ex/p> ?o }")
                .unwrap()
                .row_count(),
            1,
            "the committed insert is replayed"
        );
        assert_eq!(
            recovered
                .select("SELECT ?o WHERE { <http://ex/a> <http://ex/p> ?o }")
                .unwrap()
                .row_count(),
            0,
            "the committed delete is replayed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
