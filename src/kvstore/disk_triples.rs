//! `DiskTriples`: the on-disk triple store a Stream-mode database evaluates
//! against (contract C5 of docs/PHASE6_PLAN.md; the payload of the future
//! `Backend::Disk`, wired into `Database` by R3).
//!
//! It wraps a writable [`DiskStore`] and adds three things:
//!
//! * **Reported reads.** Every [`TripleSource`] read goes through the
//!   fallible `DiskStore::try_*` methods; a read error is reported with
//!   [`note_read_error`] (contract C2) — the statement running in the
//!   current [`ReadScope`](crate::store::read_scope::ReadScope) fails — and the
//!   read answers empty. The counts, `distinct_*` and `has_*` methods use the
//!   store's exact prefix counts; `has_*` answer `true` on an error. The same
//!   reporting reader is available on its own as [`DiskSource`].
//! * **A transaction overlay.** Between [`begin`](DiskTriples::begin) and
//!   [`commit`](DiskTriples::commit) every triple write lands in an exact
//!   [`Overlay`] and **no triple reaches the pager**: reads see the
//!   transaction's writes through an [`OverlayView`] (read-your-writes),
//!   [`rollback`](DiskTriples::rollback) just drops the overlay, and `commit`
//!   applies named-graph operations, then removals, then additions, each as
//!   one atomic pager operation. (Dictionary terms may reach the pager before
//!   commit: an orphan term is harmless.) Outside a transaction a write is
//!   applied at once (auto-commit).
//! * **A writer gate for snapshot readers.** A [`DiskReadView`] reads the
//!   committed state from other threads without borrowing the `DiskTriples`.
//!   A writer that is about to change the pager *drains* the gate: it marks
//!   it draining (a view that starts now gets [`ViewError::Expired`]), raises
//!   the cancel flag of every reader inside [`DiskReadView::with_base`] (an
//!   evaluator built `with_cancel` on it stops at its next checkpoint), waits
//!   until no reader is left, applies its change and bumps the **epoch**. A
//!   view whose epoch moved is expired, so a snapshot reader sees either the
//!   state it began with or — after retrying on a fresh view — the committed
//!   one, never a torn state. The epoch moves exactly when data changed.
//!
//! ## Durability and the broken state
//!
//! Writes reach the page cache; [`commit_point`](DiskTriples::commit_point)
//! commits them durably (one WAL-protected pager flush). Any write or flush
//! error makes the store **broken**: later writes are refused with an error,
//! reads report a read error (the statement fails) and views answer
//! [`ViewError::Io`], until the database is reopened. Dropping a broken store
//! flushes nothing. A transaction is durable once its commit reached a commit
//! point, but a crash in the middle of applying a very large one can leave
//! part of it on disk (docs/PHASE6_PLAN.md D12).
//!
//! ## Locking
//!
//! Lock order (C8): gate → pager. The gate's mutex is only ever held for a
//! few instructions; a writer holds the *draining* state (not the mutex)
//! while it takes the pager write lock. A thread that is inside
//! [`DiskReadView::with_base`] and tries to write through the `DiskTriples` of
//! the same database gets an error instead of waiting for itself.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError, RwLock};
use std::time::{Duration, Instant};

use crate::error::{GStoreError, Result};
use crate::model::id::{EntityLiteralId, PredId};
use crate::model::IdTriple;
use crate::store::read_scope::note_read_error;
use crate::store::{MutableStore, Overlay, OverlayChanges, OverlayView, TripleSource};

use super::pager::Pager;
use super::store::DiskStore;

// ---- reporting reads ---------------------------------------------------------

/// A [`TripleSource`] over a [`DiskStore`] that **reports** read errors (C2):
/// every read uses the store's fallible `try_*` method, and an error is noted
/// with [`note_read_error`] and answered as an empty list / `0` / `false`
/// (`true` for the `has_*` methods: an id is never treated as unused when in
/// doubt). Wrap the store a [`DiskReadView::with_base`] closure receives in
/// one, so a snapshot query fails on a read error instead of answering
/// partially.
#[derive(Clone, Copy)]
pub struct DiskSource<'a> {
    store: &'a DiskStore,
}

impl<'a> DiskSource<'a> {
    /// Read `store`, reporting its read errors.
    pub fn new(store: &'a DiskStore) -> DiskSource<'a> {
        DiskSource { store }
    }

    /// The store read.
    pub fn store(&self) -> &'a DiskStore {
        self.store
    }
}

#[cold]
fn report(e: io::Error) {
    note_read_error(format_args!("kvstore read failed: {e}"));
}

#[inline]
fn or_report<T: Default>(r: io::Result<T>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => {
            report(e);
            T::default()
        }
    }
}

#[inline]
fn or_true(r: io::Result<bool>) -> bool {
    match r {
        Ok(v) => v,
        Err(e) => {
            report(e);
            true
        }
    }
}

impl TripleSource for DiskSource<'_> {
    fn exists(&self, sub: EntityLiteralId, pred: PredId, obj: EntityLiteralId) -> bool {
        or_report(self.store.try_exists(sub, pred, obj))
    }
    fn po_by_s(&self, sub: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        or_report(self.store.try_po_by_s(sub))
    }
    fn o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> Vec<EntityLiteralId> {
        or_report(self.store.try_o_by_sp(sub, pred))
    }
    fn p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> Vec<PredId> {
        or_report(self.store.try_p_by_so(sub, obj))
    }
    fn ps_by_o(&self, obj: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        or_report(self.store.try_ps_by_o(obj))
    }
    fn s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> Vec<EntityLiteralId> {
        or_report(self.store.try_s_by_po(pred, obj))
    }
    fn so_by_p(&self, pred: PredId) -> Vec<(EntityLiteralId, EntityLiteralId)> {
        or_report(self.store.try_so_by_p(pred))
    }
    fn subs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        or_report(self.store.try_subs_by_p(pred))
    }
    fn objs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        or_report(self.store.try_objs_by_p(pred))
    }
    fn subject_keys(&self) -> Vec<EntityLiteralId> {
        or_report(self.store.try_subject_keys())
    }
    fn object_keys(&self) -> Vec<EntityLiteralId> {
        or_report(self.store.try_object_keys())
    }
    fn triple_count(&self) -> u64 {
        self.store.triple_count()
    }
    fn distinct_subjects(&self) -> usize {
        self.store.distinct_subject_count()
    }
    fn distinct_objects(&self) -> usize {
        self.store.distinct_object_count()
    }
    fn num_predicates(&self) -> usize {
        self.store.nonempty_predicate_count()
    }
    fn pred_card(&self, pred: PredId) -> usize {
        self.store.pred_num(pred)
    }
    fn pred_distinct_subj(&self, pred: PredId) -> usize {
        self.store.pred_subject_count(pred)
    }
    fn pred_distinct_obj(&self, pred: PredId) -> usize {
        self.store.pred_object_count(pred)
    }
    fn iter_all(&self) -> Vec<IdTriple> {
        or_report(self.store.try_iter_all())
    }

    // C1: the store's exact counts, errors reported.
    fn count_po_by_s(&self, sub: EntityLiteralId) -> u64 {
        or_report(self.store.try_count_po_by_s(sub))
    }
    fn count_o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> u64 {
        or_report(self.store.try_count_o_by_sp(sub, pred))
    }
    fn count_p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> u64 {
        or_report(self.store.try_count_p_by_so(sub, obj))
    }
    fn count_ps_by_o(&self, obj: EntityLiteralId) -> u64 {
        or_report(self.store.try_count_ps_by_o(obj))
    }
    fn count_s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> u64 {
        or_report(self.store.try_count_s_by_po(pred, obj))
    }
    fn count_so_by_p(&self, pred: PredId) -> u64 {
        or_report(self.store.try_count_so_by_p(pred))
    }
    fn distinct_p_of_s(&self, sub: EntityLiteralId) -> u64 {
        or_report(self.store.try_distinct_p_of_s(sub))
    }
    fn distinct_p_of_o(&self, obj: EntityLiteralId) -> u64 {
        or_report(self.store.try_distinct_p_of_o(obj))
    }
    fn has_subject(&self, sub: EntityLiteralId) -> bool {
        or_true(self.store.try_has_subject(sub))
    }
    fn has_object(&self, obj: EntityLiteralId) -> bool {
        or_true(self.store.try_has_object(obj))
    }
    fn has_predicate(&self, pred: PredId) -> bool {
        or_true(self.store.try_has_predicate(pred))
    }
}

// ---- long posting lists --------------------------------------------------------

/// Lists with at least this many entries are kept in the [`ListCache`].
const LIST_CACHE_MIN_LEN: usize = 2048;
/// The byte budget of the [`ListCache`] (entries are charged 4 or 8 bytes
/// each, plus [`LIST_ENTRY_OVERHEAD`]).
const LIST_CACHE_BYTES: usize = 32 << 20;
/// Bytes charged per cached list on top of its entries.
const LIST_ENTRY_OVERHEAD: usize = 64;

/// Which long posting list a [`ListCache`] entry holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ListKey {
    SByPo(PredId, EntityLiteralId),
    PsByO(EntityLiteralId),
    SoByP(PredId),
    SubsByP(PredId),
    ObjsByP(PredId),
}

/// A cached posting list.
#[derive(Clone)]
enum CachedList {
    Ids(Arc<[u32]>),
    Pairs(Arc<[(u32, u32)]>),
}

impl CachedList {
    fn bytes(&self) -> usize {
        LIST_ENTRY_OVERHEAD
            + match self {
                CachedList::Ids(v) => v.len() * 4,
                CachedList::Pairs(v) => v.len() * 8,
            }
    }
}

/// The long posting lists of the committed store (a class's members, a
/// predicate's pairs), decoded once from their B+ tree leaves and kept until
/// the store changes: the epoch they were read at is checked on every
/// lookup, and a write moves it. Bounded by [`LIST_CACHE_BYTES`] (the oldest
/// entries go first); lists shorter than [`LIST_CACHE_MIN_LEN`] are never
/// cached (a point read of them is cheap). An answer from the cache is the
/// very list the decode gave, so results and their order do not change.
#[derive(Default)]
struct ListCache {
    epoch: u64,
    bytes: usize,
    order: VecDeque<ListKey>,
    lists: HashMap<ListKey, CachedList>,
}

impl ListCache {
    /// The cached list of `key` at `epoch` (emptying a cache of an older one).
    fn get(&mut self, epoch: u64, key: ListKey) -> Option<CachedList> {
        if self.epoch != epoch {
            self.clear(epoch);
            return None;
        }
        self.lists.get(&key).cloned()
    }

    fn put(&mut self, epoch: u64, key: ListKey, list: CachedList) {
        if self.epoch != epoch {
            self.clear(epoch);
        }
        let cost = list.bytes();
        if cost > LIST_CACHE_BYTES || self.lists.contains_key(&key) {
            return;
        }
        while self.bytes + cost > LIST_CACHE_BYTES {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            if let Some(gone) = self.lists.remove(&old) {
                self.bytes -= gone.bytes();
            }
        }
        self.bytes += cost;
        self.order.push_back(key);
        self.lists.insert(key, list);
    }

    fn clear(&mut self, epoch: u64) {
        self.epoch = epoch;
        self.bytes = 0;
        self.order.clear();
        self.lists.clear();
    }
}

// ---- the writer gate -----------------------------------------------------------

/// The mutable part of the gate (behind [`DiskShared::gate`]).
#[derive(Default)]
struct GateState {
    /// Readers inside [`DiskReadView::with_base`].
    readers: usize,
    /// A writer is draining or applying: no reader may start.
    draining: bool,
    /// Id of the next registered reader.
    next_reader: u64,
    /// Each registered reader's cancel flag.
    cancels: Vec<(u64, Arc<AtomicBool>)>,
}

/// State shared by a [`DiskTriples`] and its [`DiskReadView`]s.
pub(crate) struct DiskShared {
    gate: Mutex<GateState>,
    /// Signalled when the last reader leaves.
    left: Condvar,
    /// Bumped after every applied change of the store's contents.
    epoch: AtomicU64,
    /// Set when the `DiskTriples` was dropped or closed.
    closed: AtomicBool,
    /// The first write or flush error; set once, never cleared.
    broken: OnceLock<String>,
}

thread_local! {
    /// The gates (by address) this thread is reading through, innermost last:
    /// a write through one of them from this thread would wait for itself.
    static READING: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
}

/// How long a writer waits for snapshot readers before it logs a warning
/// (it keeps waiting: a reader that never checks its cancel flag delays it).
const DRAIN_WARN_AFTER: Duration = Duration::from_secs(10);

impl DiskShared {
    fn new() -> DiskShared {
        DiskShared {
            gate: Mutex::new(GateState::default()),
            left: Condvar::new(),
            epoch: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            broken: OnceLock::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, GateState> {
        self.gate.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn key(&self) -> usize {
        self as *const DiskShared as usize
    }

    /// `Err` when this thread is inside one of this store's own snapshot
    /// reads ([`DiskReadView::with_base`]): a drain would wait for itself.
    fn refuse_reentry(&self) -> Result<()> {
        let me = self.key();
        if READING
            .try_with(|r| r.borrow().contains(&me))
            .unwrap_or(false)
        {
            return Err(GStoreError::Database(
                "a write to the on-disk store from inside one of its own snapshot reads \
                 (DiskReadView::with_base) would wait for itself"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Writer side: mark the gate draining, cancel every reader inside it and
    /// wait until none is left. The returned guard ends the drain when
    /// dropped, bumping the epoch first if its `changed` flag is set.
    fn drain(&self) -> Result<Drain<'_>> {
        self.refuse_reentry()?;
        let mut g = self.lock();
        g.draining = true;
        let started = Instant::now();
        let mut warned = false;
        while g.readers > 0 {
            for (_, cancel) in &g.cancels {
                cancel.store(true, Ordering::SeqCst);
            }
            let (guard, _) = self
                .left
                .wait_timeout(g, Duration::from_millis(250))
                .unwrap_or_else(PoisonError::into_inner);
            g = guard;
            if !warned && g.readers > 0 && started.elapsed() >= DRAIN_WARN_AFTER {
                warned = true;
                crate::gs_warn!(
                    "a write to the on-disk store has waited {:?} for {} snapshot reader(s) \
                     to honour their cancel flag",
                    started.elapsed(),
                    g.readers
                );
            }
        }
        Ok(Drain {
            shared: self,
            changed: false,
        })
    }
}

/// An ongoing drain (see [`DiskShared::drain`]).
struct Drain<'a> {
    shared: &'a DiskShared,
    /// Whether the store's contents changed under this drain (bumps the epoch).
    changed: bool,
}

impl Drop for Drain<'_> {
    fn drop(&mut self) {
        if self.changed {
            self.shared.epoch.fetch_add(1, Ordering::SeqCst);
        }
        self.shared.lock().draining = false;
    }
}

/// One reader inside the gate; leaving (drop, also on unwind) deregisters it.
struct ReaderGuard<'a> {
    shared: &'a DiskShared,
    id: u64,
}

impl Drop for ReaderGuard<'_> {
    fn drop(&mut self) {
        let me = self.shared.key();
        let _ = READING.try_with(|r| {
            let mut r = r.borrow_mut();
            if let Some(i) = r.iter().rposition(|&k| k == me) {
                r.remove(i);
            }
        });
        let mut g = self.shared.lock();
        g.readers -= 1;
        g.cancels.retain(|(id, _)| *id != self.id);
        if g.readers == 0 {
            self.shared.left.notify_all();
        }
    }
}

// ---- snapshot views --------------------------------------------------------------

/// Why a [`DiskReadView`] could not serve a read.
#[derive(Debug)]
pub enum ViewError {
    /// The store changed (or a writer is changing it) since the view was
    /// taken: re-run the read on the live database.
    Expired,
    /// The [`DiskTriples`] was dropped or closed.
    Closed,
    /// The store could not be read, or a write error broke it.
    Io(io::Error),
}

impl fmt::Display for ViewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ViewError::Expired => f.write_str("the snapshot expired: the database changed"),
            ViewError::Closed => f.write_str("the database was closed"),
            ViewError::Io(e) => write!(f, "the snapshot could not be read: {e}"),
        }
    }
}

impl std::error::Error for ViewError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ViewError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<ViewError> for GStoreError {
    fn from(e: ViewError) -> GStoreError {
        match e {
            ViewError::Io(e) => GStoreError::Io(e),
            other => GStoreError::Database(other.to_string()),
        }
    }
}

/// A snapshot of a [`DiskTriples`]' committed state at one epoch, readable
/// from any thread without borrowing the `DiskTriples` (it holds the shared
/// pager and gate). It never sees an open transaction's writes. Cheap to
/// clone; `Send + Sync`.
#[derive(Clone)]
pub struct DiskReadView {
    pager: Arc<RwLock<Pager>>,
    shared: Arc<DiskShared>,
    begin_epoch: u64,
}

impl fmt::Debug for DiskReadView {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiskReadView")
            .field("begin_epoch", &self.begin_epoch)
            .field("epoch", &self.shared.epoch.load(Ordering::SeqCst))
            .field("closed", &self.shared.closed.load(Ordering::SeqCst))
            .finish()
    }
}

impl DiskReadView {
    /// The epoch this view reads.
    pub fn begin_epoch(&self) -> u64 {
        self.begin_epoch
    }

    /// Whether a read started now could succeed (not expired, closed or
    /// broken). Advisory: a writer may start right after.
    pub fn is_current(&self) -> bool {
        !self.shared.closed.load(Ordering::SeqCst)
            && self.shared.broken.get().is_none()
            && self.shared.epoch.load(Ordering::SeqCst) == self.begin_epoch
            && !self.shared.lock().draining
    }

    /// Run `f` over the store as of this view's epoch. `f` gets a read-only
    /// [`DiskStore`] built for this call (wrap it in a [`DiskSource`] so read
    /// errors are reported) and the reader's cancel flag: a writer raises it
    /// when it needs the gate, and `f` should stop soon after (an
    /// [`Evaluator`](crate::query::Evaluator) built `with_cancel` on it does).
    ///
    /// Errors: [`ViewError::Expired`] if the gate is draining or the epoch
    /// moved — checked before `f` runs and again after it returned (a reader
    /// that was cancelled gets `Expired`, whatever `f` returned) —
    /// [`ViewError::Closed`] after the `DiskTriples` was dropped, and
    /// [`ViewError::Io`] if the store is broken or the view could not be
    /// built. Nothing is retried here: on `Expired` the caller re-runs its
    /// read on the live database (or on a fresh view).
    pub fn with_base<R>(
        &self,
        f: impl FnOnce(&DiskStore, &Arc<AtomicBool>) -> R,
    ) -> std::result::Result<R, ViewError> {
        let shared = &*self.shared;
        let (guard, cancel) = {
            let mut g = shared.lock();
            if shared.closed.load(Ordering::SeqCst) {
                return Err(ViewError::Closed);
            }
            if let Some(e) = shared.broken.get() {
                return Err(ViewError::Io(broken_io(e)));
            }
            if g.draining || shared.epoch.load(Ordering::SeqCst) != self.begin_epoch {
                return Err(ViewError::Expired);
            }
            g.readers += 1;
            let id = g.next_reader;
            g.next_reader += 1;
            let cancel = Arc::new(AtomicBool::new(false));
            g.cancels.push((id, Arc::clone(&cancel)));
            let _ = READING.try_with(|r| r.borrow_mut().push(shared.key()));
            (ReaderGuard { shared, id }, cancel)
        };
        let store = DiskStore::from_shared_pager(Arc::clone(&self.pager)).map_err(|e| match e {
            GStoreError::Io(e) => ViewError::Io(e),
            other => ViewError::Io(io::Error::other(other.to_string())),
        })?;
        let out = f(&store, &cancel);
        drop(store);
        let expired = {
            let g = shared.lock();
            g.draining
                || cancel.load(Ordering::SeqCst)
                || shared.epoch.load(Ordering::SeqCst) != self.begin_epoch
        };
        drop(guard);
        if expired {
            Err(ViewError::Expired)
        } else {
            Ok(out)
        }
    }
}

fn broken_io(e: &str) -> io::Error {
    io::Error::other(format!(
        "the on-disk store is broken by an earlier write error ({e}); reopen the database"
    ))
}

fn broken_error(e: &str) -> GStoreError {
    GStoreError::Database(format!(
        "the on-disk store is broken by an earlier write error ({e}); reopen the database"
    ))
}

// ---- DiskTriples -------------------------------------------------------------------

/// A named-graph operation queued in a transaction (applied first at commit,
/// in order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NamedOp {
    /// Insert (`true`) or delete (`false`).
    pub insert: bool,
    /// The graph IRI's entity id.
    pub graph: EntityLiteralId,
    /// The quad's triple.
    pub triple: IdTriple,
}

/// An open transaction: the default-graph overlay and the queued named-graph
/// operations.
#[derive(Debug, Default)]
pub struct DiskTxn {
    overlay: Overlay,
    named_ops: Vec<NamedOp>,
}

impl DiskTxn {
    /// The default-graph changes so far.
    pub fn overlay(&self) -> &Overlay {
        &self.overlay
    }

    /// The named-graph operations queued so far, in order.
    pub fn named_ops(&self) -> &[NamedOp] {
        &self.named_ops
    }

    /// Whether the transaction holds no change at all.
    pub fn is_empty(&self) -> bool {
        self.overlay.is_empty() && self.named_ops.is_empty()
    }
}

/// No write failure is injected.
const NO_INJECTION: u64 = u64::MAX;

/// The on-disk triple store of a Stream-mode database (see the module docs):
/// a [`TripleSource`] + [`MutableStore`] over a [`DiskStore`], with a
/// transaction overlay, a writer gate for [`DiskReadView`]s and a broken
/// state. `Send + Sync`; reads take `&self`, writes `&mut self`.
pub struct DiskTriples {
    store: DiskStore,
    shared: Arc<DiskShared>,
    txn: Option<DiskTxn>,
    /// Whether applied writes wait for a commit point.
    dirty: AtomicBool,
    /// The first write error not yet taken by [`take_error`](Self::take_error)
    /// (the [`MutableStore`] methods can only answer `false`).
    error: Mutex<Option<String>>,
    /// Debug: the number of pager writes to let through before failing one.
    fail_after: AtomicU64,
    /// Long posting lists of the committed store (see [`ListCache`]).
    lists: Mutex<ListCache>,
}

impl fmt::Debug for DiskTriples {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DiskTriples")
            .field("triples", &self.store.triple_count())
            .field("epoch", &self.epoch())
            .field("in_transaction", &self.txn.is_some())
            .field("dirty", &self.is_dirty())
            .field("broken", &self.broken())
            .finish()
    }
}

impl DiskTriples {
    /// Wrap a writable store. A read-only view
    /// ([`DiskStore::from_shared_pager`]) is refused.
    pub fn new(store: DiskStore) -> Result<DiskTriples> {
        if store.is_read_only() {
            return Err(GStoreError::Database(
                "DiskTriples needs a writable DiskStore, not a read-only view".into(),
            ));
        }
        // Statistics the open rebuilt (a file older than them) are written
        // by the first commit point — `sync`, a checkpoint, the close — so
        // the next open does not rebuild them again.
        let dirty = store.stats_rebuilt();
        Ok(DiskTriples {
            store,
            shared: Arc::new(DiskShared::new()),
            txn: None,
            dirty: AtomicBool::new(dirty),
            error: Mutex::new(None),
            fail_after: AtomicU64::new(NO_INJECTION),
            lists: Mutex::new(ListCache::default()),
        })
    }

    /// Open (or create) `kvstore.kv` at `path` with a `cache_pages`-page cache.
    pub fn open<P: AsRef<Path>>(path: P, cache_pages: usize) -> Result<DiskTriples> {
        DiskTriples::new(DiskStore::open(path, cache_pages)?)
    }

    /// The underlying store: the **committed** state (an open transaction's
    /// writes are not in it). Its own [`TripleSource`] impl swallows read
    /// errors; read through `self` (or a [`DiskSource`]) to report them.
    ///
    /// Refused once the store is broken (C5): a commit that failed part-way
    /// may have left some of its operations in the page cache, so the pages
    /// hold neither the old nor the new committed state until a reopen.
    pub fn disk_store(&self) -> Result<&DiskStore> {
        self.check_open()?;
        Ok(&self.store)
    }

    /// The pager shared with the dictionary and the views; refused once the
    /// store is broken, as [`disk_store`](Self::disk_store).
    pub fn shared_pager(&self) -> Result<Arc<RwLock<Pager>>> {
        self.check_open()?;
        Ok(self.store.shared_pager())
    }

    /// The current epoch: bumped after every applied change of the contents.
    pub fn epoch(&self) -> u64 {
        self.shared.epoch.load(Ordering::SeqCst)
    }

    /// A snapshot view of the committed state as of now (see
    /// [`DiskReadView`]).
    pub fn read_view(&self) -> DiskReadView {
        DiskReadView {
            pager: self.store.shared_pager(),
            shared: Arc::clone(&self.shared),
            begin_epoch: self.epoch(),
        }
    }

    /// The first write or flush error, if the store is broken.
    pub fn broken(&self) -> Option<&str> {
        self.shared.broken.get().map(String::as_str)
    }

    /// A check that fails once this store is broken: for a dictionary
    /// sharing its pager, which must not write through it then
    /// ([`DiskDict::with_write_check`](super::DiskDict::with_write_check)).
    pub fn write_check(&self) -> super::disk_dict::WriteCheck {
        let shared = Arc::clone(&self.shared);
        Arc::new(move || match shared.broken.get() {
            Some(e) => Err(broken_error(e)),
            None => Ok(()),
        })
    }

    /// Whether applied writes wait for a [`commit_point`](Self::commit_point).
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::SeqCst)
    }

    /// Collect (and clear) the first write error recorded since the last call
    /// (the [`MutableStore`] methods report a failed write as "not applied").
    pub fn take_error(&self) -> Option<String> {
        self.error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// Whether a transaction is open.
    pub fn in_transaction(&self) -> bool {
        self.txn.is_some()
    }

    /// The open transaction, if any.
    pub fn transaction(&self) -> Option<&DiskTxn> {
        self.txn.as_ref()
    }

    /// Whether a `vstree.kv` built beside the store is stale for what this
    /// store reads: stale on disk, or an open transaction adds triples (the
    /// tree, built from committed data, lacks them; read-your-writes would
    /// prune them away). A transaction's removals keep the tree a sound
    /// superset. Use the VS-tree only while this is `false` (C9).
    pub fn vstree_stale(&self) -> bool {
        self.store.vstree_stale()
            || self
                .txn
                .as_ref()
                .is_some_and(|t| !t.overlay.added().is_empty())
    }

    /// Whether the open transaction adds default-graph triples (which a
    /// `vstree.kv` built from committed data lacks; see
    /// [`vstree_stale`](Self::vstree_stale)).
    pub fn transaction_adds(&self) -> bool {
        self.txn
            .as_ref()
            .is_some_and(|t| !t.overlay.added().is_empty())
    }

    /// The predicates with at least one triple in what this store reads
    /// (the committed state and an open transaction's changes), ascending.
    pub fn predicates(&self) -> Vec<PredId> {
        let mut limit = self.store.predicate_stat_limit();
        if let Some(o) = self.overlay() {
            if let Some(p) = o.added().predicates().last() {
                limit = limit.max(p + 1);
            }
        }
        (0..limit).filter(|&p| self.pred_card(p) > 0).collect()
    }

    /// Record that `vstree.kv` was rebuilt from the current contents
    /// (passthrough; persisted at the next commit point).
    pub fn mark_vstree_fresh(&mut self) {
        self.store.mark_vstree_fresh();
        self.dirty.store(true, Ordering::SeqCst);
    }

    /// Debug: make the next pager write (a triple or quad write, or a commit
    /// point's flush) fail, which breaks the store.
    #[doc(hidden)]
    pub fn debug_fail_next_write(&self) {
        self.debug_fail_write_after(0);
    }

    /// Debug: let `n` pager writes through, then fail the next one.
    #[doc(hidden)]
    pub fn debug_fail_write_after(&self, n: u64) {
        self.fail_after.store(n, Ordering::SeqCst);
    }

    // ---- errors ------------------------------------------------------------

    fn check_open(&self) -> Result<()> {
        match self.shared.broken.get() {
            Some(e) => Err(broken_error(e)),
            None => Ok(()),
        }
    }

    /// Break the store with `e` (the first error wins) and return `e`.
    fn fail(&self, e: GStoreError) -> GStoreError {
        let msg = e.to_string();
        let _ = self.shared.broken.set(msg.clone());
        self.error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(msg);
        e
    }

    /// Note `e` for [`take_error`](Self::take_error) without breaking anything
    /// (a refused write).
    fn note(&self, e: &GStoreError) {
        self.error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert_with(|| e.to_string());
    }

    /// Consume one step of an injected failure.
    fn injected(&self) -> Result<()> {
        let n = self.fail_after.load(Ordering::SeqCst);
        match n {
            NO_INJECTION => Ok(()),
            0 => {
                self.fail_after.store(NO_INJECTION, Ordering::SeqCst);
                Err(GStoreError::Database(
                    "injected write failure (debug_fail_next_write)".into(),
                ))
            }
            n => {
                self.fail_after.store(n - 1, Ordering::SeqCst);
                Ok(())
            }
        }
    }

    /// One pager write (an atomic store operation).
    fn apply<R>(&mut self, f: impl FnOnce(&mut DiskStore) -> Result<R>) -> Result<R> {
        self.injected()?;
        f(&mut self.store)
    }

    /// Run `f` (pager writes that report whether data changed) under a drain
    /// of the gate; any error breaks the store.
    fn under_gate(&mut self, f: impl FnOnce(&mut DiskTriples) -> Result<bool>) -> Result<bool> {
        let shared = Arc::clone(&self.shared);
        let mut drain = shared.drain()?;
        match f(self) {
            Ok(changed) => {
                drain.changed = changed;
                if changed {
                    self.dirty.store(true, Ordering::SeqCst);
                }
                Ok(changed)
            }
            Err(e) => {
                // Earlier operations of `f` may have reached the pages.
                drain.changed = true;
                Err(self.fail(e))
            }
        }
    }

    // ---- writes ------------------------------------------------------------

    /// Insert `t` (ids interned in this store's dictionary): into the open
    /// transaction's overlay, or at once (auto-commit). Returns `true` exactly
    /// when `t` was not there before. An error breaks the store.
    pub fn insert_triple(&mut self, t: IdTriple) -> Result<bool> {
        self.check_open()?;
        if let Some(txn) = self.txn.as_mut() {
            let store = &self.store;
            let r = txn
                .overlay
                .try_insert(t, |t| store.try_exists(t.sub, t.pred, t.obj));
            return r.map_err(|e| self.fail(GStoreError::Io(e)));
        }
        match self.store.try_exists(t.sub, t.pred, t.obj) {
            Err(e) => Err(self.fail(GStoreError::Io(e))),
            Ok(true) => Ok(false),
            Ok(false) => self.under_gate(|dt| dt.apply(|st| st.insert_ids(t))),
        }
    }

    /// Remove `t`: from the open transaction's view, or at once (auto-commit).
    /// Returns `true` exactly when `t` was there before. An error breaks the
    /// store.
    pub fn remove_triple(&mut self, t: IdTriple) -> Result<bool> {
        self.check_open()?;
        if let Some(txn) = self.txn.as_mut() {
            let store = &self.store;
            let r = txn
                .overlay
                .try_remove(t, |t| store.try_exists(t.sub, t.pred, t.obj));
            return r.map_err(|e| self.fail(GStoreError::Io(e)));
        }
        match self.store.try_exists(t.sub, t.pred, t.obj) {
            Err(e) => Err(self.fail(GStoreError::Io(e))),
            Ok(false) => Ok(false),
            Ok(true) => self.under_gate(|dt| dt.apply(|st| st.delete_ids(t))),
        }
    }

    /// Insert many triples; returns how many were new. Outside a transaction
    /// the new ones are applied under **one** drain of the gate (one epoch
    /// step).
    pub fn insert_many(&mut self, triples: impl IntoIterator<Item = IdTriple>) -> Result<u64> {
        self.check_open()?;
        if self.txn.is_some() {
            let mut n = 0;
            for t in triples {
                n += u64::from(self.insert_triple(t)?);
            }
            return Ok(n);
        }
        let mut todo: Vec<IdTriple> = triples.into_iter().collect();
        todo.sort_unstable();
        todo.dedup();
        let mut fresh = Vec::with_capacity(todo.len());
        for t in todo {
            match self.store.try_exists(t.sub, t.pred, t.obj) {
                Ok(true) => {}
                Ok(false) => fresh.push(t),
                Err(e) => return Err(self.fail(GStoreError::Io(e))),
            }
        }
        if fresh.is_empty() {
            return Ok(0);
        }
        let mut n = 0u64;
        self.under_gate(|dt| {
            for &t in &fresh {
                n += u64::from(dt.apply(|st| st.insert_ids(t))?);
            }
            Ok(n > 0)
        })?;
        Ok(n)
    }

    /// Remove many triples; returns how many existed. Outside a transaction
    /// the present ones are removed under **one** drain of the gate (one
    /// epoch step) — `CLEAR DEFAULT` of a Stream-mode database.
    pub fn remove_many(&mut self, triples: impl IntoIterator<Item = IdTriple>) -> Result<u64> {
        self.check_open()?;
        if self.txn.is_some() {
            let mut n = 0;
            for t in triples {
                n += u64::from(self.remove_triple(t)?);
            }
            return Ok(n);
        }
        let mut todo: Vec<IdTriple> = triples.into_iter().collect();
        todo.sort_unstable();
        todo.dedup();
        let mut present = Vec::with_capacity(todo.len());
        for t in todo {
            match self.store.try_exists(t.sub, t.pred, t.obj) {
                Ok(true) => present.push(t),
                Ok(false) => {}
                Err(e) => return Err(self.fail(GStoreError::Io(e))),
            }
        }
        if present.is_empty() {
            return Ok(0);
        }
        let mut n = 0u64;
        self.under_gate(|dt| {
            for &t in &present {
                n += u64::from(dt.apply(|st| st.delete_ids(t))?);
            }
            Ok(n > 0)
        })?;
        Ok(n)
    }

    /// Persist a named-graph operation: queued in the open transaction
    /// (applied first at commit, in order), or applied at once. The
    /// in-memory named graphs decide whether something changed; a no-op here
    /// leaves the store (and the epoch) alone.
    pub fn named_op(&mut self, insert: bool, graph: EntityLiteralId, t: IdTriple) -> Result<()> {
        self.check_open()?;
        if let Some(txn) = self.txn.as_mut() {
            txn.named_ops.push(NamedOp {
                insert,
                graph,
                triple: t,
            });
            return Ok(());
        }
        match self.store.try_quad_exists(graph, t) {
            Err(e) => return Err(self.fail(GStoreError::Io(e))),
            Ok(exists) if exists == insert => return Ok(()),
            Ok(_) => {}
        }
        self.under_gate(|dt| {
            dt.apply(|st| {
                if insert {
                    st.insert_quad_ids(graph, t)
                } else {
                    st.delete_quad_ids(graph, t)
                }
            })
        })?;
        Ok(())
    }

    // ---- transactions ----------------------------------------------------------

    /// Open a transaction: from now on triple writes only change its overlay.
    pub fn begin(&mut self) -> Result<()> {
        self.check_open()?;
        if self.txn.is_some() {
            return Err(GStoreError::Database(
                "a transaction is already open on this store".into(),
            ));
        }
        self.txn = Some(DiskTxn::default());
        Ok(())
    }

    /// Commit the open transaction under the gate: its named-graph operations
    /// (in order), then its removals, then its additions, each one atomic
    /// pager operation; then the epoch moves (if anything changed). The
    /// changes are durable at the next [`commit_point`](Self::commit_point).
    ///
    /// A commit from inside one of this store's own snapshot reads
    /// ([`DiskReadView::with_base`] on this thread) is refused before anything
    /// is taken: the transaction stays open, to commit after leaving the
    /// view. Any other failure breaks the store (the transaction is gone
    /// either way).
    pub fn commit(&mut self) -> Result<()> {
        if self.txn.is_none() {
            return Err(GStoreError::Database("no transaction is open".into()));
        }
        self.shared.refuse_reentry()?;
        let txn = self
            .txn
            .take()
            .ok_or_else(|| GStoreError::Database("no transaction is open".into()))?;
        self.check_open()?;
        let DiskTxn {
            mut overlay,
            named_ops,
        } = txn;
        let changes = overlay.drain();
        if changes.is_empty() && named_ops.is_empty() {
            return Ok(());
        }
        self.under_gate(|dt| dt.apply_changes(&named_ops, &changes))?;
        Ok(())
    }

    fn apply_changes(&mut self, named_ops: &[NamedOp], changes: &OverlayChanges) -> Result<bool> {
        let mut changed = false;
        for op in named_ops {
            changed |= self.apply(|st| {
                if op.insert {
                    st.insert_quad_ids(op.graph, op.triple)
                } else {
                    st.delete_quad_ids(op.graph, op.triple)
                }
            })?;
        }
        for &t in &changes.removed {
            changed |= self.apply(|st| st.delete_ids(t))?;
        }
        for &t in &changes.added {
            changed |= self.apply(|st| st.insert_ids(t))?;
        }
        Ok(changed)
    }

    /// Drop the open transaction (the pager is never touched). Returns whether
    /// one was open.
    pub fn rollback(&mut self) -> bool {
        self.txn.take().is_some()
    }

    /// Commit every applied write durably (one WAL-protected pager flush), if
    /// there is any. Refused once broken; a failing flush breaks the store.
    pub fn commit_point(&self) -> Result<()> {
        self.check_open()?;
        if !self.dirty.load(Ordering::SeqCst) {
            return Ok(());
        }
        let flushed = self.injected().and_then(|()| self.store.commit());
        match flushed {
            Ok(()) => {
                self.dirty.store(false, Ordering::SeqCst);
                Ok(())
            }
            Err(e) => Err(self.fail(e)),
        }
    }

    /// Close the store: drain the gate (views answer [`ViewError::Closed`]
    /// from now on), drop an open transaction and commit applied writes,
    /// reporting a flush error (dropping does the same, best effort).
    pub fn close(mut self) -> Result<()> {
        self.txn = None;
        self.shut();
        if let Some(e) = self.shared.broken.get() {
            return Err(broken_error(e));
        }
        if self.dirty.load(Ordering::SeqCst) {
            self.store.commit()?;
            self.dirty.store(false, Ordering::SeqCst);
        }
        Ok(())
    }

    /// Drain the gate for good and mark the views closed.
    fn shut(&self) {
        if self.shared.closed.load(Ordering::SeqCst) {
            return;
        }
        match self.shared.drain() {
            Ok(drain) => {
                self.shared.closed.store(true, Ordering::SeqCst);
                drop(drain);
            }
            // Dropped from inside one of its own snapshot reads: the views
            // still have to stop.
            Err(_) => self.shared.closed.store(true, Ordering::SeqCst),
        }
    }

    // ---- reads -------------------------------------------------------------

    /// Whether reads may proceed; a broken store reports a read error.
    #[inline]
    fn readable(&self) -> bool {
        match self.shared.broken.get() {
            None => true,
            Some(e) => {
                note_read_error(broken_io(e));
                false
            }
        }
    }

    fn list_cache(&self) -> MutexGuard<'_, ListCache> {
        self.lists.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// An id list of the committed store: from the [`ListCache`], or `read`
    /// (whose result is cached when it is long).
    fn cached_ids(&self, key: ListKey, read: impl FnOnce() -> Vec<u32>) -> Vec<u32> {
        let epoch = self.epoch();
        // The cache's guard is released at the end of this statement: the
        // hit is copied out of its `Arc` after that, so concurrent readers
        // of long lists do not queue on the copy.
        let hit = self.list_cache().get(epoch, key);
        if let Some(CachedList::Ids(hit)) = hit {
            return hit.to_vec();
        }
        let list = read();
        if list.len() >= LIST_CACHE_MIN_LEN {
            self.list_cache()
                .put(epoch, key, CachedList::Ids(Arc::from(list.as_slice())));
        }
        list
    }

    /// A pair list of the committed store (see [`cached_ids`](Self::cached_ids)).
    fn cached_pairs(
        &self,
        key: ListKey,
        read: impl FnOnce() -> Vec<(u32, u32)>,
    ) -> Vec<(u32, u32)> {
        let epoch = self.epoch();
        // Copied after the guard is released (see `cached_ids`).
        let hit = self.list_cache().get(epoch, key);
        if let Some(CachedList::Pairs(hit)) = hit {
            return hit.to_vec();
        }
        let list = read();
        if list.len() >= LIST_CACHE_MIN_LEN {
            self.list_cache()
                .put(epoch, key, CachedList::Pairs(Arc::from(list.as_slice())));
        }
        list
    }

    /// The transaction overlay to read through, if it holds anything.
    #[inline]
    fn overlay(&self) -> Option<&Overlay> {
        self.txn
            .as_ref()
            .map(|t| &t.overlay)
            .filter(|o| !o.is_empty())
    }
}

impl Drop for DiskTriples {
    /// Drain the gate, mark the views closed and commit applied writes (best
    /// effort; nothing when broken). An open transaction is dropped.
    fn drop(&mut self) {
        if self.shared.closed.load(Ordering::SeqCst) {
            return;
        }
        self.shut();
        if self.shared.broken.get().is_none() && self.dirty.load(Ordering::SeqCst) {
            let _ = self.store.commit();
        }
    }
}

/// A long-list read: through the overlay when a transaction holds changes,
/// else from the [`ListCache`] or the store (`$cache` is `cached_ids` or
/// `cached_pairs`); a broken store answers empty after reporting.
macro_rules! read_list {
    ($self:ident, $cache:ident, $key:expr, |$src:ident| $body:expr) => {{
        if !$self.readable() {
            return Vec::new();
        }
        let base = DiskSource::new(&$self.store);
        match $self.overlay() {
            Some(overlay) => {
                let $src = OverlayView::new(&base, overlay);
                $body
            }
            None => $self.$cache($key, || {
                let $src = base;
                $body
            }),
        }
    }};
}

/// Read through the overlay when a transaction holds changes, else straight
/// from the store; a broken store answers `$empty` after reporting.
macro_rules! read {
    ($self:ident, $empty:expr, |$src:ident| $body:expr) => {{
        if !$self.readable() {
            return $empty;
        }
        let base = DiskSource::new(&$self.store);
        match $self.overlay() {
            Some(overlay) => {
                let $src = OverlayView::new(&base, overlay);
                $body
            }
            None => {
                let $src = base;
                $body
            }
        }
    }};
}

impl TripleSource for DiskTriples {
    fn exists(&self, sub: EntityLiteralId, pred: PredId, obj: EntityLiteralId) -> bool {
        read!(self, false, |s| s.exists(sub, pred, obj))
    }
    fn po_by_s(&self, sub: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        read!(self, Vec::new(), |s| s.po_by_s(sub))
    }
    fn o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> Vec<EntityLiteralId> {
        read!(self, Vec::new(), |s| s.o_by_sp(sub, pred))
    }
    fn p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> Vec<PredId> {
        read!(self, Vec::new(), |s| s.p_by_so(sub, obj))
    }
    fn ps_by_o(&self, obj: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        read_list!(self, cached_pairs, ListKey::PsByO(obj), |s| s.ps_by_o(obj))
    }
    fn s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> Vec<EntityLiteralId> {
        read_list!(self, cached_ids, ListKey::SByPo(pred, obj), |s| s
            .s_by_po(pred, obj))
    }
    fn so_by_p(&self, pred: PredId) -> Vec<(EntityLiteralId, EntityLiteralId)> {
        read_list!(self, cached_pairs, ListKey::SoByP(pred), |s| s
            .so_by_p(pred))
    }
    fn subs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        read_list!(self, cached_ids, ListKey::SubsByP(pred), |s| s
            .subs_by_p(pred))
    }
    fn objs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        read_list!(self, cached_ids, ListKey::ObjsByP(pred), |s| s
            .objs_by_p(pred))
    }
    fn subject_keys(&self) -> Vec<EntityLiteralId> {
        read!(self, Vec::new(), |s| s.subject_keys())
    }
    fn object_keys(&self) -> Vec<EntityLiteralId> {
        read!(self, Vec::new(), |s| s.object_keys())
    }
    fn triple_count(&self) -> u64 {
        read!(self, 0, |s| s.triple_count())
    }
    fn distinct_subjects(&self) -> usize {
        read!(self, 0, |s| s.distinct_subjects())
    }
    fn distinct_objects(&self) -> usize {
        read!(self, 0, |s| s.distinct_objects())
    }
    fn num_predicates(&self) -> usize {
        read!(self, 0, |s| s.num_predicates())
    }
    fn pred_card(&self, pred: PredId) -> usize {
        read!(self, 0, |s| s.pred_card(pred))
    }
    fn pred_distinct_subj(&self, pred: PredId) -> usize {
        read!(self, 0, |s| s.pred_distinct_subj(pred))
    }
    fn pred_distinct_obj(&self, pred: PredId) -> usize {
        read!(self, 0, |s| s.pred_distinct_obj(pred))
    }
    fn iter_all(&self) -> Vec<IdTriple> {
        read!(self, Vec::new(), |s| s.iter_all())
    }

    fn count_po_by_s(&self, sub: EntityLiteralId) -> u64 {
        read!(self, 0, |s| s.count_po_by_s(sub))
    }
    fn count_o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> u64 {
        read!(self, 0, |s| s.count_o_by_sp(sub, pred))
    }
    fn count_p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> u64 {
        read!(self, 0, |s| s.count_p_by_so(sub, obj))
    }
    fn count_ps_by_o(&self, obj: EntityLiteralId) -> u64 {
        read!(self, 0, |s| s.count_ps_by_o(obj))
    }
    fn count_s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> u64 {
        read!(self, 0, |s| s.count_s_by_po(pred, obj))
    }
    fn count_so_by_p(&self, pred: PredId) -> u64 {
        read!(self, 0, |s| s.count_so_by_p(pred))
    }
    fn distinct_p_of_s(&self, sub: EntityLiteralId) -> u64 {
        read!(self, 0, |s| s.distinct_p_of_s(sub))
    }
    fn distinct_p_of_o(&self, obj: EntityLiteralId) -> u64 {
        read!(self, 0, |s| s.distinct_p_of_o(obj))
    }
    fn has_subject(&self, sub: EntityLiteralId) -> bool {
        read!(self, true, |s| s.has_subject(sub))
    }
    fn has_object(&self, obj: EntityLiteralId) -> bool {
        read!(self, true, |s| s.has_object(obj))
    }
    fn has_predicate(&self, pred: PredId) -> bool {
        read!(self, true, |s| s.has_predicate(pred))
    }
}

impl MutableStore for DiskTriples {
    /// [`insert_triple`](DiskTriples::insert_triple); an error answers `false`
    /// and is kept for [`take_error`](DiskTriples::take_error).
    fn insert(&mut self, t: IdTriple) -> bool {
        self.insert_triple(t).unwrap_or_else(|e| {
            self.note(&e);
            false
        })
    }

    /// [`remove_triple`](DiskTriples::remove_triple); an error answers
    /// `false` and is kept for [`take_error`](DiskTriples::take_error).
    fn remove(&mut self, t: IdTriple) -> bool {
        self.remove_triple(t).unwrap_or_else(|e| {
            self.note(&e);
            false
        })
    }

    /// [`insert_many`](DiskTriples::insert_many); an error is kept for
    /// [`take_error`](DiskTriples::take_error).
    fn bulk_load(&mut self, triples: Vec<IdTriple>) {
        if let Err(e) = self.insert_many(triples) {
            self.note(&e);
        }
    }
}

#[allow(dead_code)]
fn assert_send_sync<T: Send + Sync>() {}
#[allow(dead_code)]
fn disk_triples_are_send_and_sync() {
    assert_send_sync::<DiskTriples>();
    assert_send_sync::<DiskReadView>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::read_scope::ReadScope;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "gstore_disk_triples_{tag}_{}.kv",
            std::process::id()
        ));
        cleanup(&p);
        p
    }

    fn cleanup(p: &Path) {
        let _ = std::fs::remove_file(p);
        let mut wal = p.as_os_str().to_owned();
        wal.push(".wal");
        let _ = std::fs::remove_file(wal);
    }

    /// A store whose ids `0..n` are interned entities and predicates.
    fn seeded(path: &Path, n: u32) -> DiskTriples {
        let mut ds = DiskStore::open(path, 256).unwrap();
        for i in 0..n {
            ds.intern_entity(&format!("<e{i}>")).unwrap();
            ds.intern_predicate(&format!("<p{i}>")).unwrap();
        }
        DiskTriples::new(ds).unwrap()
    }

    #[test]
    fn auto_commit_moves_the_epoch_only_on_change() {
        let p = tmp("epoch");
        let mut dt = seeded(&p, 4);
        let t = IdTriple::new(0, 1, 2);
        assert_eq!(dt.epoch(), 0);
        assert!(dt.insert_triple(t).unwrap());
        assert_eq!(dt.epoch(), 1);
        assert!(!dt.insert_triple(t).unwrap());
        assert_eq!(dt.epoch(), 1);
        assert!(!dt.remove_triple(IdTriple::new(3, 3, 3)).unwrap());
        assert_eq!(dt.epoch(), 1);
        assert!(dt.remove_triple(t).unwrap());
        assert_eq!(dt.epoch(), 2);
        drop(dt);
        cleanup(&p);
    }

    #[test]
    fn a_write_from_inside_a_view_is_refused_not_deadlocked() {
        let p = tmp("reenter");
        let dt = Arc::new(Mutex::new(seeded(&p, 4)));
        let view = dt.lock().unwrap().read_view();
        let r = view.with_base(|_, _| {
            dt.lock()
                .unwrap()
                .insert_triple(IdTriple::new(0, 0, 1))
                .map_err(|e| e.to_string())
        });
        let inner = r.expect("the view itself stays valid");
        assert!(inner.unwrap_err().contains("wait for itself"));
        drop(dt);
        cleanup(&p);
    }

    #[test]
    fn long_lists_are_cached_until_the_store_changes() {
        let p = tmp("lists");
        let mut dt = seeded(&p, 4);
        let n = LIST_CACHE_MIN_LEN as u32 + 10;
        for i in 0..n {
            let e = dt.store.intern_entity(&format!("<m{i}>")).unwrap();
            dt.insert_triple(IdTriple::new(e, 1, 2)).unwrap();
        }
        let first = dt.s_by_po(1, 2);
        assert_eq!(first.len(), n as usize);
        assert_eq!(dt.list_cache().lists.len(), 1, "a long list is cached");
        assert_eq!(dt.s_by_po(1, 2), first, "served from the cache");
        assert_eq!(dt.so_by_p(1).len(), n as usize);
        // A short list is not cached.
        assert!(dt.s_by_po(0, 1).is_empty());
        assert_eq!(dt.list_cache().lists.len(), 2);

        // A write moves the epoch: the cached lists are not used again.
        assert!(dt.insert_triple(IdTriple::new(3, 1, 2)).unwrap());
        let after = dt.s_by_po(1, 2);
        assert_eq!(after.len(), first.len() + 1);
        assert!(after.windows(2).all(|w| w[0] < w[1]), "ascending");
        assert!(after.contains(&3));
        // A transaction's changes read through the overlay, not the cache.
        dt.begin().unwrap();
        assert!(dt.remove_triple(IdTriple::new(3, 1, 2)).unwrap());
        assert_eq!(dt.s_by_po(1, 2), first);
        dt.rollback();
        assert_eq!(dt.s_by_po(1, 2), after);
        drop(dt);
        cleanup(&p);
    }

    #[test]
    fn broken_reads_fail_the_scope() {
        let p = tmp("broken");
        let mut dt = seeded(&p, 4);
        dt.debug_fail_next_write();
        assert!(dt.insert_triple(IdTriple::new(0, 0, 1)).is_err());
        assert!(dt.broken().is_some());
        let scope = ReadScope::enter();
        assert!(dt.po_by_s(0).is_empty());
        assert!(dt.has_subject(0), "has_* answer true when in doubt");
        assert!(scope.take_error().unwrap().contains("broken"));
        drop(dt);
        cleanup(&p);
    }
}
