//! An out-of-core dictionary backend over the on-disk B+trees.
//!
//! Mirrors gStore resolving terms through its `entity2id`/`id2entity` (and the
//! literal/predicate) trees via the buffer cache, rather than holding the whole
//! string dictionary in RAM. A [`DiskDict`] plugs into [`crate::dict::Dictionary`]
//! as its [`DiskTermSource`] (contract C4); the query engine then runs over
//! `&Dictionary` unchanged while only the *looked-up* strings become resident.
//!
//! ## Residency / memory
//! * **id → string** (result rows, FILTER values): read from the `id2*`
//!   [`IvArray`] in place ([`IvArray::get_with`], no intermediate copy) into a
//!   shared `Arc<str>`, kept in a bounded [`TermCache`] (64 MiB by default).
//! * **string → id** (query constants): the `*2id` B+tree
//!   ([`BTree::get_with`]), kept in a bounded [`KeyCache`] (16 MiB by default).
//!   Only hits are cached, so a later intern can never be shadowed.
//! * [`prefetch`](DiskTermSource::prefetch) and
//!   [`resolve_sorted`](DiskTermSource::resolve_sorted) take ids sorted
//!   ascending and read them with [`IvArray::for_each_key`]: one directory
//!   page serves 32 consecutive ids, pages are visited in file order, and only
//!   the requested slots are decoded (an unrequested neighbour's overflow chain
//!   is never read, so its damage cannot fail the batch).
//!
//! Both caches are bounded by bytes and evict with CLOCK, so a long-running
//! reader stays within its budget however many terms it touches; strings are
//! handed out as `Arc<str>` (nothing is leaked).
//!
//! ## Interning (Stream mode)
//! [`intern`](DiskTermSource::intern) adds a term through the shared pager:
//! its id is the lowest free id of its space when the file has the
//! `FREE_IDS` feature and one is free, else the next id (the header's count
//! root, the space's high-water mark) — written as `id2*`, then `*2id`, then
//! the roots, in **one** pager operation under the pager write lock — so a
//! crash or an error leaves the term wholly there or wholly absent, never an
//! id the count does not cover. It touches neither the triple indexes nor
//! the `DiskTriples` writer gate (C8: gate → pager is never inverted): a term
//! is written before any triple that uses it, and one whose triple never
//! comes (a rolled-back transaction, a crash) is a harmless orphan. Every new
//! term bumps the [`generation`](DiskTermSource::generation); since an intern
//! only *adds* a key and an id, nothing cached changes meaning, so both
//! caches are updated in place (the new term goes in) instead of being
//! emptied.
//!
//! ## Freeing (R4, `FREE_IDS`)
//! [`free`](DiskTermSource::free) removes a term — its `*2id` entry and its
//! `id2*` slot — and releases its id in the same pager operation: into the
//! `FREE_IDS` tree (header slot 22; keys `space tag ‖ be32(index)`, so the
//! lowest free id of a space is the tree's first key with its tag) with the
//! space's `FREE_COUNT_*` root, or, for the space's highest id, by lowering
//! the count root — and every free id that becomes the highest with it (the
//! trim cascade). A free changes what an id and a key mean, so it moves both
//! the generation and the caches' epoch (emptying both caches). A file
//! without the feature (every pre-Phase-6 file) keeps reclamation off: `free`
//! answers `Ok(false)` and writes nothing; `gcompact --enable-reclaim`
//! upgrades it. The caller (`DiskTriples::free_ids`, under the writer gate)
//! guarantees the term is unreferenced.
//!
//! Read errors are returned (`Err`); the [`Dictionary`](crate::dict::Dictionary)
//! front end notes the ones it cannot return in the running statement's read
//! scope (contract C2).

use std::ops::ControlFlow;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use crate::dict::cache::{KeyCache, TermCache, DEFAULT_KEY_CACHE_BYTES, DEFAULT_TERM_CACHE_BYTES};
use crate::dict::{CacheStats, DiskTermSource, Space};
use crate::error::{GStoreError, Result};

use super::bptree::{be32, de32, BTree};
use super::format::{
    ENTITY2ID, FEATURE_FREE_IDS, FREE_COUNT_E, FREE_COUNT_L, FREE_COUNT_P, FREE_IDS, ID2ENTITY,
    ID2LITERAL, ID2PREDICATE, LITERAL2ID, PREDICATE2ID, ROOT_ENTITY_COUNT, ROOT_LITERAL_COUNT,
    ROOT_PRED_COUNT,
};
use super::ivarray::{self, IvArray};
use super::pager::{Pager, PagerOp};

// ---- the free-id list (FREE_IDS, slot 22) ------------------------------------

/// The `FREE_IDS` tree: 5-byte keys `space tag ‖ be32(index)` (index = id
/// minus the space's first id), empty values.
pub(crate) fn free_tree() -> BTree {
    BTree::with_fixed_shape(FREE_IDS, 5, 0)
}

/// The `FREE_IDS` key tag of `space`.
fn space_tag(space: Space) -> u8 {
    match space {
        Space::Entity => b'E',
        Space::Literal => b'L',
        Space::Predicate => b'P',
    }
}

fn free_key(space: Space, index: u32) -> [u8; 5] {
    let mut k = [0u8; 5];
    k[0] = space_tag(space);
    k[1..5].copy_from_slice(&be32(index));
    k
}

/// The header root holding the number of free ids of `space`.
pub(crate) fn free_count_root(space: Space) -> usize {
    match space {
        Space::Entity => FREE_COUNT_E,
        Space::Literal => FREE_COUNT_L,
        Space::Predicate => FREE_COUNT_P,
    }
}

/// Whether the file behind `pager` keeps a free-id list (feature
/// `FREE_IDS`): only then are ids freed and reused.
pub(crate) fn frees_ids(pager: &Pager) -> bool {
    pager.features() & FEATURE_FREE_IDS != 0
}

/// The id-space roots of `space`: `(allocated, free)` — the high-water mark
/// (one past the highest index ever handed out and not trimmed) and the
/// number of free indices below it.
pub(crate) fn space_roots(pager: &Pager, space: Space) -> (u64, u64) {
    (
        pager.root(count_root(space)),
        if frees_ids(pager) {
            pager.root(free_count_root(space))
        } else {
            0
        },
    )
}

/// Hand out the id of a new term of `space`, inside the caller's pager
/// operation: the lowest free id when the file keeps a free list and one is
/// free, else the next one (bounded like the in-memory dictionary, and by
/// the `id2*` array's capacity). The roots are updated; the caller writes
/// the term.
pub(crate) fn allocate_id(pager: &mut Pager, space: Space) -> Result<u32> {
    if frees_ids(pager) && pager.root(free_count_root(space)) > 0 {
        let tree = free_tree();
        match tree.first_with_prefix(pager, &[space_tag(space)])? {
            Some((k, _)) if k.len() == 5 => {
                let index = de32(&k[1..5]);
                tree.delete(pager, &k)?;
                let n = pager.root(free_count_root(space));
                pager.set_root(free_count_root(space), n.saturating_sub(1));
                return Ok(space.first_id() + index);
            }
            Some(_) => {
                return Err(GStoreError::Database(
                    "corrupt free-id list: a key has the wrong length".into(),
                ))
            }
            // The count said "free" but the list is empty: repair the count.
            None => pager.set_root(free_count_root(space), 0),
        }
    }
    let slot = count_root(space);
    let next = pager.root(slot);
    let id = match space {
        Space::Entity => crate::dict::checked_entity_id(next)?,
        Space::Literal => crate::dict::checked_literal_id(next)?,
        Space::Predicate => crate::dict::checked_predicate_id(next)?,
    };
    if next >= ivarray::CAPACITY as u64 {
        return Err(GStoreError::Database(format!(
            "the on-disk {} dictionary is full ({} ids)",
            space.name(),
            ivarray::CAPACITY
        )));
    }
    pager.set_root(slot, next + 1);
    Ok(id)
}

/// Release index `index` of `space` (its term already removed), inside the
/// caller's pager operation: the highest index lowers the high-water mark,
/// with every free index that becomes the highest (the trim cascade); any
/// other goes into the free list.
fn release_index(pager: &mut Pager, space: Space, index: u32) -> Result<()> {
    let tree = free_tree();
    let mut alloc = pager.root(count_root(space));
    let mut free = pager.root(free_count_root(space));
    if u64::from(index) + 1 == alloc {
        alloc -= 1;
        while alloc > 0 {
            let k = free_key(space, (alloc - 1) as u32);
            if !tree.delete(pager, &k)? {
                break;
            }
            free = free.saturating_sub(1);
            alloc -= 1;
        }
        pager.set_root(count_root(space), alloc);
    } else {
        tree.insert(pager, &free_key(space, index), b"")?;
        free += 1;
    }
    pager.set_root(free_count_root(space), free);
    Ok(())
}

/// Remove the term of id `id` in `space` from `key_tree` (`*2id`) and
/// `array` (`id2*`) and release the id, inside the caller's pager
/// operation. `Ok(false)` — nothing written — when the file keeps no free
/// list or `id` names no term.
pub(crate) fn free_term_in(
    pager: &mut Pager,
    space: Space,
    id: u32,
    key_tree: &BTree,
    array: &IvArray,
) -> Result<bool> {
    if !frees_ids(pager) {
        return Ok(false);
    }
    let Some(index) = id.checked_sub(space.first_id()) else {
        return Ok(false);
    };
    if u64::from(index) >= pager.root(count_root(space)) {
        return Ok(false);
    }
    let Some(key) = array.get(pager, index)? else {
        return Ok(false);
    };
    key_tree.delete(pager, &key)?;
    array.delete(pager, index)?;
    release_index(pager, space, index)?;
    Ok(true)
}

/// Turn the free-id list on for the file behind `pager` (a new database, or
/// `gcompact --enable-reclaim`): the feature bit and `MAGIC_V2` reach the
/// file with the next committed flush, together with the (empty) list.
pub(crate) fn enable_free_ids(pager: &mut Pager) -> Result<()> {
    if frees_ids(pager) {
        return Ok(());
    }
    pager.enable_feature(FEATURE_FREE_IDS)?;
    for space in Space::ALL {
        pager.set_root(free_count_root(space), 0);
    }
    Ok(())
}

/// Out-of-core dictionary: resolves str↔id from the on-disk B+trees on demand.
pub struct DiskDict {
    /// Shared with the owning `DiskStore`, so both read through one page cache.
    /// Resolution only takes the read guard, so it runs alongside other readers.
    pager: Arc<RwLock<Pager>>,
    entity2id: BTree,
    literal2id: BTree,
    predicate2id: BTree,
    id2entity: IvArray,
    id2literal: IvArray,
    id2predicate: IvArray,
    /// The allocated term counts (the high-water mark of each space, minus
    /// its first id: holes included); set by [`intern`](DiskTermSource::intern)
    /// and [`free`](DiskTermSource::free) once the pages hold the change.
    entity_count: AtomicUsize,
    literal_count: AtomicUsize,
    pred_count: AtomicUsize,
    /// The free ids below each space's high-water mark (`FREE_COUNT_*`).
    free_counts: [AtomicUsize; 3],
    terms: TermCache,
    keys: KeyCache,
    /// The public generation (see [`DiskTermSource::generation`]): bumped by
    /// every term interned through this backend.
    generation: AtomicU64,
    /// The caches' epoch: moves only when a cached key or id may have
    /// changed meaning (a freed id, R4), which empties both caches. An intern
    /// only adds, so it leaves this alone and updates the caches in place.
    cache_epoch: AtomicU64,
    /// Whether [`intern`](DiskTermSource::intern) may write new terms (see
    /// [`writable`](Self::writable)); a read-only backend answers existing
    /// keys only and refuses new ones.
    writable: bool,
    /// Asked before every write (see [`with_write_check`](Self::with_write_check)).
    write_check: Option<WriteCheck>,
}

/// What a writable [`DiskDict`] asks before it writes a term: `Err` refuses
/// the write (the store sharing the pager is broken).
pub type WriteCheck = Arc<dyn Fn() -> Result<()> + Send + Sync>;

impl DiskDict {
    /// Build a backend sharing `pager` with its `DiskStore`. The `BTree` handles
    /// are copies of the store's dictionary trees; `*_count` are the live term
    /// counts captured when the backend is created. The caches get the default
    /// budgets (see [`with_cache_budgets`](Self::with_cache_budgets)).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pager: Arc<RwLock<Pager>>,
        entity2id: BTree,
        literal2id: BTree,
        predicate2id: BTree,
        id2entity: IvArray,
        id2literal: IvArray,
        id2predicate: IvArray,
        entity_count: usize,
        literal_count: usize,
        pred_count: usize,
    ) -> DiskDict {
        DiskDict {
            pager,
            entity2id,
            literal2id,
            predicate2id,
            id2entity,
            id2literal,
            id2predicate,
            entity_count: AtomicUsize::new(entity_count),
            literal_count: AtomicUsize::new(literal_count),
            pred_count: AtomicUsize::new(pred_count),
            free_counts: Default::default(),
            terms: TermCache::new(DEFAULT_TERM_CACHE_BYTES),
            keys: KeyCache::new(DEFAULT_KEY_CACHE_BYTES),
            generation: AtomicU64::new(0),
            cache_epoch: AtomicU64::new(0),
            writable: false,
            write_check: None,
        }
    }

    /// Let this backend intern new terms through the shared pager (Stream
    /// mode: the dictionary of a database whose store is a
    /// [`DiskTriples`](super::DiskTriples) over the same pager). A backend is
    /// read-only by default — [`DiskStore::lazy_dictionary`](super::DiskStore::lazy_dictionary)
    /// and views must never write.
    pub fn writable(mut self) -> DiskDict {
        self.writable = true;
        self
    }

    /// A backend over the dictionary structures of the `kvstore.kv` behind
    /// `pager` (e.g. [`DiskStore::shared_pager`](super::DiskStore::shared_pager)),
    /// with the term counts read from its header now.
    pub fn open_shared(pager: Arc<RwLock<Pager>>) -> Result<DiskDict> {
        let (counts, free) = {
            let p = pager.read().unwrap_or_else(|e| e.into_inner());
            let dense = |slot: usize, what: &str| -> Result<usize> {
                let n = p.root(slot);
                if n > ivarray::CAPACITY as u64 {
                    return Err(GStoreError::Database(format!(
                        "corrupt kvstore header: {what} count {n} exceeds the id capacity {}",
                        ivarray::CAPACITY
                    )));
                }
                Ok(n as usize)
            };
            let counts = (
                dense(ROOT_ENTITY_COUNT, "entity")?,
                dense(ROOT_LITERAL_COUNT, "literal")?,
                dense(ROOT_PRED_COUNT, "predicate")?,
            );
            let free = Space::ALL.map(|s| space_roots(&p, s).1 as usize);
            (counts, free)
        };
        let dd = DiskDict::new(
            pager,
            BTree::new(ENTITY2ID),
            BTree::new(LITERAL2ID),
            BTree::new(PREDICATE2ID),
            IvArray::new(ID2ENTITY),
            IvArray::new(ID2LITERAL),
            IvArray::new(ID2PREDICATE),
            counts.0,
            counts.1,
            counts.2,
        );
        for (cell, n) in dd.free_counts.iter().zip(free) {
            cell.store(n, Ordering::Release);
        }
        Ok(dd)
    }

    /// Set the free-id counts (a store building a backend over its pager
    /// passes the header's).
    pub(crate) fn with_free_counts(self, free: [usize; 3]) -> DiskDict {
        for (cell, n) in self.free_counts.iter().zip(free) {
            cell.store(n, Ordering::Release);
        }
        self
    }

    /// The index of `space` in per-space arrays.
    fn space_index(space: Space) -> usize {
        match space {
            Space::Entity => 0,
            Space::Literal => 1,
            Space::Predicate => 2,
        }
    }

    /// The number of free ids of `space` below its high-water mark.
    pub fn free_count(&self, space: Space) -> usize {
        self.free_counts[Self::space_index(space)].load(Ordering::Acquire)
    }

    /// The allocated count (high-water mark) of `space`.
    fn allocated(&self, space: Space) -> usize {
        self.count_cell(space).load(Ordering::Acquire)
    }

    /// Re-read `space`'s roots after a write (under the write guard).
    fn reload_roots(&self, pager: &Pager, space: Space) {
        let (alloc, free) = space_roots(pager, space);
        self.count_cell(space)
            .store(alloc as usize, Ordering::Release);
        self.free_counts[Self::space_index(space)].store(free as usize, Ordering::Release);
    }

    /// Ask `check` before every term this backend writes, and refuse the
    /// write when it fails: the dictionary of a Stream-mode database shares
    /// its pager with a [`DiskTriples`](super::DiskTriples), and once that
    /// store is broken (a commit failed part-way, its applied pages still in
    /// the cache) nothing may write through the pager — ending a write
    /// operation can flush the cache, making the failed commit durable.
    pub fn with_write_check(mut self, check: WriteCheck) -> DiskDict {
        self.write_check = Some(check);
        self
    }

    /// Replace the caches with empty ones of the given byte budgets
    /// (`id → string` and `string → id`).
    pub fn with_cache_budgets(mut self, term_bytes: usize, key_bytes: usize) -> DiskDict {
        self.terms = TermCache::new(term_bytes);
        self.keys = KeyCache::new(key_bytes);
        self
    }

    /// The `id2*` array of `space`.
    fn array(&self, space: Space) -> &IvArray {
        match space {
            Space::Entity => &self.id2entity,
            Space::Literal => &self.id2literal,
            Space::Predicate => &self.id2predicate,
        }
    }

    /// The allocated-count cell of `space`.
    fn count_cell(&self, space: Space) -> &AtomicUsize {
        match space {
            Space::Entity => &self.entity_count,
            Space::Literal => &self.literal_count,
            Space::Predicate => &self.pred_count,
        }
    }

    /// The allocated counts (high-water marks) as `(entities, literals,
    /// predicates)`.
    pub fn counts(&self) -> (usize, usize, usize) {
        (
            self.allocated(Space::Entity),
            self.allocated(Space::Literal),
            self.allocated(Space::Predicate),
        )
    }

    /// The allocated and free counts of every space — what a store compares
    /// with its header to tell whether this backend is current.
    pub fn header_state(&self) -> [(usize, usize); 3] {
        Space::ALL.map(|s| (self.allocated(s), self.free_count(s)))
    }

    /// Write the new term `key` of `space` inside the caller's pager
    /// operation: checked again under the write lock (an existing key answers
    /// its id and `false`), then its id — the lowest free one, else the next
    /// (see [`allocate_id`]) — as `id2*`, `*2id` and the roots. Returns
    /// `(id, new)`.
    fn intern_in(&self, pager: &mut Pager, space: Space, key: &str) -> Result<(u32, bool)> {
        let tree = self.tree(space);
        let found = tree.get_with(pager, key.as_bytes(), |v| {
            <[u8; 4]>::try_from(v).map(u32::from_be_bytes)
        })?;
        match found {
            Some(Ok(id)) => return Ok((id, false)),
            Some(Err(_)) => {
                return Err(GStoreError::Database(format!(
                    "corrupt {} dictionary entry for key {key:?}",
                    space.name()
                )))
            }
            None => {}
        }
        let id = allocate_id(pager, space)?;
        // The array key is the id relative to the space's first id (literal
        // ids are public ids, offset by LITERAL_FIRST_ID).
        self.array(space)
            .insert(pager, id - space.first_id(), key.as_bytes())?;
        tree.insert(pager, key.as_bytes(), &be32(id))?;
        Ok((id, true))
    }

    fn tree(&self, space: Space) -> &BTree {
        match space {
            Space::Entity => &self.entity2id,
            Space::Literal => &self.literal2id,
            Space::Predicate => &self.predicate2id,
        }
    }

    /// The `id2*` array of `space` and the array key of public id `id`
    /// (`None` for an id outside the space's allocated range — bounded by
    /// the high-water mark, not the live count: freed ids leave holes).
    fn slot(&self, space: Space, id: u32) -> Option<(&IvArray, u32)> {
        let key = id.checked_sub(space.first_id())?;
        if key as usize >= self.allocated(space) {
            return None;
        }
        Some((self.array(space), key))
    }

    fn pager(&self) -> std::sync::RwLockReadGuard<'_, Pager> {
        // A reader that panicked cannot have left the pager half-written.
        self.pager.read().unwrap_or_else(|e| e.into_inner())
    }

    fn fresh_terms(&self) {
        self.terms
            .check_generation(self.cache_epoch.load(Ordering::Acquire));
    }

    fn fresh_keys(&self) {
        self.keys
            .check_generation(self.cache_epoch.load(Ordering::Acquire));
    }

    /// Read the strings of `keys` (array keys, ascending, within `arr`'s
    /// range), handing each to `f` with its public id. Only the requested
    /// slots are decoded ([`IvArray::for_each_key`]): directory pages are
    /// visited in file order, once per run of keys they serve, and an
    /// unrequested neighbour's overflow chain is never read.
    fn read_keys(
        &self,
        pager: &Pager,
        arr: &IvArray,
        first: u32,
        keys: &[u32],
        f: &mut dyn FnMut(u32, Arc<str>),
    ) -> Result<()> {
        arr.for_each_key(pager, keys, |k, v| f(first + k, decode(v)))
    }
}

/// The header root holding the allocated count of `space`.
fn count_root(space: Space) -> usize {
    match space {
        Space::Entity => ROOT_ENTITY_COUNT,
        Space::Literal => ROOT_LITERAL_COUNT,
        Space::Predicate => ROOT_PRED_COUNT,
    }
}

/// Dictionary keys are written from `&str`, so they are UTF-8; a damaged one
/// still decodes (lossily) rather than failing the whole query.
fn decode(bytes: &[u8]) -> Arc<str> {
    match std::str::from_utf8(bytes) {
        Ok(s) => Arc::from(s),
        Err(_) => Arc::from(String::from_utf8_lossy(bytes).as_ref()),
    }
}

impl DiskTermSource for DiskDict {
    fn lookup(&self, space: Space, key: &str) -> Result<Option<u32>> {
        self.fresh_keys();
        if let Some(id) = self.keys.get(space, key) {
            return Ok(Some(id));
        }
        // The literal tree stores the *public* id (offset by LITERAL_FIRST_ID).
        let found = self
            .tree(space)
            .get_with(&self.pager(), key.as_bytes(), |v| {
                <[u8; 4]>::try_from(v).map(u32::from_be_bytes)
            })?;
        match found {
            None => Ok(None),
            Some(Ok(id)) => {
                self.keys.insert(space, key, id);
                Ok(Some(id))
            }
            Some(Err(_)) => Err(GStoreError::Database(format!(
                "corrupt {} dictionary entry for key {key:?}",
                space.name()
            ))),
        }
    }

    fn resolve(&self, space: Space, id: u32) -> Result<Option<Arc<str>>> {
        self.fresh_terms();
        if let Some(s) = self.terms.get(space, id) {
            return Ok(Some(s));
        }
        let Some((arr, key)) = self.slot(space, id) else {
            return Ok(None);
        };
        let s = arr.get_with(&self.pager(), key, decode)?;
        if let Some(s) = &s {
            self.terms.insert(space, id, Arc::clone(s));
        }
        Ok(s)
    }

    /// An existing key answers its id (no write). A new one is written
    /// through the shared pager as one operation under its write lock (see
    /// the module docs), then cached and counted, and the generation moves.
    /// An error (an exhausted id space, an over-long key, a write failure)
    /// rolls the operation back: nothing of the term stays in the pages.
    fn intern(&self, space: Space, key: &str) -> Result<u32> {
        if let Some(id) = self.lookup(space, key)? {
            return Ok(id);
        }
        if !self.writable {
            return Err(GStoreError::Database(format!(
                "this out-of-core dictionary is read-only: cannot intern a new {} term",
                space.name()
            )));
        }
        if let Some(check) = &self.write_check {
            check()?;
        }
        let (id, new) = {
            let mut guard = self.pager.write().unwrap_or_else(|e| e.into_inner());
            let mut op = PagerOp::begin(&mut guard);
            // A panic (a bug) is rolled back like an error instead of
            // unwinding through the write guard, which would poison the
            // pager for every reader sharing it.
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.intern_in(&mut op, space, key)
            }))
            .unwrap_or_else(|_| {
                Err(GStoreError::Database(
                    "interning a dictionary term panicked and was rolled back".into(),
                ))
            });
            match outcome {
                Ok(r) => {
                    op.commit()?;
                    if r.1 {
                        // Visible to readers only now that the pages hold it.
                        self.reload_roots(&guard, space);
                    }
                    r
                }
                Err(e) => {
                    drop(op); // rolls the pages and the header back
                    return Err(e);
                }
            }
        };
        if new {
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
        self.fresh_keys();
        self.keys.insert(space, key, id);
        self.fresh_terms();
        self.terms.insert(space, id, Arc::from(key));
        Ok(id)
    }

    /// Remove the term of `id` and free the id (see the module docs), as one
    /// pager operation under the write lock; then the generation and the
    /// caches' epoch move. `Ok(false)` when the file keeps no free list (a
    /// pre-Phase-6 file: reclamation is off) or `id` names no term. A
    /// read-only backend, or one whose store is broken, refuses.
    fn free(&self, space: Space, id: u32) -> Result<bool> {
        if !self.writable {
            return Err(GStoreError::Database(format!(
                "this out-of-core dictionary is read-only: cannot free a {} id",
                space.name()
            )));
        }
        if let Some(check) = &self.write_check {
            check()?;
        }
        let freed = {
            let mut guard = self.pager.write().unwrap_or_else(|e| e.into_inner());
            if !frees_ids(&guard) {
                return Ok(false);
            }
            let mut op = PagerOp::begin(&mut guard);
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                free_term_in(&mut op, space, id, self.tree(space), self.array(space))
            }))
            .unwrap_or_else(|_| {
                Err(GStoreError::Database(
                    "freeing a dictionary id panicked and was rolled back".into(),
                ))
            });
            match outcome {
                Ok(freed) => {
                    op.commit()?;
                    if freed {
                        self.reload_roots(&guard, space);
                    }
                    freed
                }
                Err(e) => {
                    drop(op); // rolls the pages and the header back
                    return Err(e);
                }
            }
        };
        if freed {
            // An id and a key changed meaning: both caches are emptied.
            self.cache_epoch.fetch_add(1, Ordering::AcqRel);
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
        Ok(freed)
    }

    /// One cache probe per id; the misses are read run by run in id order
    /// and cached.
    fn resolve_sorted(&self, space: Space, ids: &[u32]) -> Result<Vec<Option<Arc<str>>>> {
        self.fresh_terms();
        let first = space.first_id();
        let mut out: Vec<Option<Arc<str>>> = Vec::with_capacity(ids.len());
        // (position in `ids`, array key) of each miss, ascending.
        let mut missing: Vec<(usize, u32)> = Vec::new();
        let mut arr = None;
        for (i, &id) in ids.iter().enumerate() {
            let hit = self.terms.get(space, id);
            if hit.is_none() {
                if let Some((a, key)) = self.slot(space, id) {
                    arr = Some(a);
                    missing.push((i, key));
                }
            }
            out.push(hit);
        }
        if let (Some(arr), false) = (arr, missing.is_empty()) {
            let keys: Vec<u32> = missing.iter().map(|&(_, k)| k).collect();
            let mut at = missing.iter().peekable();
            let pager = self.pager();
            self.read_keys(&pager, arr, first, &keys, &mut |id, s| {
                while let Some(&&(i, k)) = at.peek() {
                    at.next();
                    if first + k == id {
                        out[i] = Some(Arc::clone(&s));
                        break;
                    }
                }
                self.terms.insert(space, id, s);
            })?;
        }
        Ok(out)
    }

    fn prefetch(&self, space: Space, ids: &[u32]) -> Result<()> {
        self.fresh_terms();
        let first = space.first_id();
        let mut missing: Vec<u32> = Vec::new();
        let mut arr = None;
        for &id in ids {
            let Some((a, key)) = self.slot(space, id) else {
                continue;
            };
            arr = Some(a);
            if !self.terms.contains(space, id) {
                missing.push(key);
            }
        }
        let (Some(arr), false) = (arr, missing.is_empty()) else {
            return Ok(());
        };
        let pager = self.pager();
        self.read_keys(&pager, arr, first, &missing, &mut |id, s| {
            self.terms.insert(space, id, s)
        })
    }

    fn for_each(
        &self,
        space: Space,
        from: u32,
        f: &mut dyn FnMut(u32, &str) -> ControlFlow<()>,
    ) -> Result<()> {
        let first = space.first_id();
        let end = self.allocated(space) as u32;
        let start = from.saturating_sub(first);
        let Some((arr, _)) = self.slot(space, first + start) else {
            return Ok(());
        };
        let pager = self.pager();
        arr.for_each_range(&pager, start, end, |k, v| match std::str::from_utf8(v) {
            Ok(s) => f(first + k, s),
            Err(_) => f(first + k, &String::from_utf8_lossy(v)),
        })
    }

    /// Live terms: the allocated ids minus the free ones.
    fn count(&self, space: Space) -> usize {
        self.allocated(space).saturating_sub(self.free_count(space))
    }

    fn id_limit(&self, space: Space) -> u32 {
        space.first_id() + self.allocated(space) as u32
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    fn cache_stats(&self) -> CacheStats {
        let (th, tm, te, ti) = self.terms.cache().counters();
        let (kh, km, ke, ki) = self.keys.counters();
        CacheStats {
            term_entries: self.terms.cache().len(),
            term_bytes: self.terms.cache().bytes(),
            term_budget: self.terms.cache().budget(),
            key_entries: self.keys.len(),
            key_bytes: self.keys.bytes(),
            key_budget: self.keys.budget(),
            hits: th + kh,
            misses: tm + km,
            evictions: te + ke,
            invalidations: ti.max(ki),
        }
    }
}

impl std::fmt::Debug for DiskDict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskDict")
            .field("entity_count", &self.count(Space::Entity))
            .field("literal_count", &self.count(Space::Literal))
            .field("pred_count", &self.count(Space::Predicate))
            .field("resident_strings", &self.terms.cache().len())
            .field("resident_bytes", &self.terms.cache().bytes())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::Dictionary;
    use crate::kvstore::DiskStore;
    use crate::model::id::LITERAL_FIRST_ID;
    use crate::model::Term;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "gstore_disk_dict_{name}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    fn store(name: &str, n: u32) -> (std::path::PathBuf, DiskStore) {
        let path = tmp(name);
        let mut content = String::new();
        for i in 0..n {
            content.push_str(&format!(
                "<http://ex/s{i}> <http://ex/p{}> \"literal number {i}\" .\n",
                i % 5
            ));
        }
        let ds = DiskStore::build_str(&path, 64, &content).unwrap();
        (path, ds)
    }

    #[test]
    fn every_id_round_trips_and_matches_the_eager_load() {
        let (path, ds) = store("roundtrip", 500);
        let dd = DiskDict::open_shared(ds.shared_pager()).unwrap();
        let lazy = Dictionary::from_backing(Arc::new(dd));
        let eager = ds.dictionary().unwrap();
        for space in Space::ALL {
            assert_eq!(lazy.id_limit(space), eager.id_limit(space));
            for id in space.first_id()..lazy.id_limit(space) {
                let t = lazy.resolve(space, id).unwrap();
                assert_eq!(Some(&*t), eager.resolve(space, id).as_deref());
                assert_eq!(lazy.id_of(space, &t), Some(id), "{space:?} {id}");
            }
            // Past the end: nothing, and no error.
            assert_eq!(lazy.try_resolve(space, lazy.id_limit(space)).unwrap(), None);
        }
        assert_eq!(
            lazy.term_id(&Term::plain_literal("literal number 7")),
            eager.term_id(&Term::plain_literal("literal number 7"))
        );
        assert_eq!(lazy.entity_id("<http://ex/none>"), None);
        // A backed dictionary materializes into an equal in-memory one.
        let m = lazy.materialize().unwrap();
        assert_eq!(
            bincode::serialize(&m).unwrap(),
            bincode::serialize(&eager).unwrap()
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn prefetch_reads_runs_into_the_cache() {
        let (path, ds) = store("prefetch", 300);
        let dd = Arc::new(DiskDict::open_shared(ds.shared_pager()).unwrap());
        let ids: Vec<u32> = vec![0, 1, 2, 40, 41, 299, 7000];
        dd.prefetch(Space::Entity, &ids).unwrap();
        let st = dd.cache_stats();
        assert_eq!(st.term_entries, 6, "the six valid ids, nothing else");
        let before = dd.cache_stats().misses;
        for &id in &ids[..6] {
            assert!(dd.resolve(Space::Entity, id).unwrap().is_some());
        }
        assert_eq!(dd.cache_stats().misses, before, "prefetched ids hit");
        // Literal ids are public ids.
        let lit = LITERAL_FIRST_ID + 5;
        dd.prefetch(Space::Literal, &[lit]).unwrap();
        assert_eq!(
            dd.resolve(Space::Literal, lit).unwrap().as_deref(),
            Some("\"literal number 5\"")
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn caches_stay_within_a_small_budget() {
        let (path, ds) = store("budget", 2000);
        let dd = Arc::new(
            DiskDict::open_shared(ds.shared_pager())
                .unwrap()
                .with_cache_budgets(16 << 10, 16 << 10),
        );
        for round in 0..3 {
            for id in 0..2000 {
                let s = dd.resolve(Space::Entity, id).unwrap().unwrap();
                assert_eq!(&*s, format!("<http://ex/s{id}>"));
                let key = format!("\"literal number {id}\"");
                assert_eq!(
                    dd.lookup(Space::Literal, &key).unwrap(),
                    Some(LITERAL_FIRST_ID + id),
                    "round {round}"
                );
                let st = dd.cache_stats();
                assert!(st.term_bytes <= st.term_budget);
                assert!(st.key_bytes <= st.key_budget);
            }
        }
        assert!(dd.cache_stats().evictions > 0);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_read_only_backend_refuses_writes() {
        let (path, ds) = store("readonly", 10);
        let mut d = ds.lazy_dictionary();
        assert!(d.try_intern_entity("<http://ex/new>").is_err());
        assert_eq!(
            d.try_intern_entity("<http://ex/s1>").unwrap(),
            d.entity_id("<http://ex/s1>").unwrap(),
            "an existing term answers its id"
        );
        let scope = crate::store::read_scope::ReadScope::enter();
        assert_eq!(d.free_entity("<http://ex/s1>"), None);
        assert!(scope.take_error().is_some(), "a refused free is reported");
        std::fs::remove_file(&path).ok();
    }

    fn wal(path: &std::path::Path) -> std::path::PathBuf {
        let mut w = path.as_os_str().to_owned();
        w.push(".wal");
        std::path::PathBuf::from(w)
    }

    #[test]
    fn a_writable_backend_interns_through_the_shared_pager() {
        let (path, mut ds) = store("intern", 10);
        let (e0, l0, p0) = (ds.entity_num(), ds.literal_num(), ds.predicate_num());
        let dd = Arc::new(DiskDict::open_shared(ds.shared_pager()).unwrap().writable());
        let mut d = Dictionary::from_backing(dd.clone());
        let g0 = d.generation();

        // An existing key answers its id and writes nothing.
        let s1 = d.try_intern_entity("<http://ex/s1>").unwrap();
        assert_eq!(Some(s1), d.entity_id("<http://ex/s1>"));
        assert_eq!(d.generation(), g0);

        // New terms take the next id of their space and are readable at once
        // (cached in place), and the generation moves.
        let e = d.try_intern_entity("<http://ex/new>").unwrap();
        let l = d.try_intern_literal("\"fresh\"").unwrap();
        let p = d.try_intern_predicate("<http://ex/pnew>").unwrap();
        assert_eq!(e as usize, e0);
        assert_eq!(l as usize, LITERAL_FIRST_ID as usize + l0);
        assert_eq!(p as usize, p0);
        assert_ne!(d.generation(), g0);
        assert_eq!(d.term(e).as_deref(), Some("<http://ex/new>"));
        assert_eq!(d.term(l).as_deref(), Some("\"fresh\""));
        assert_eq!(d.pred(p).as_deref(), Some("<http://ex/pnew>"));
        assert_eq!(d.entity_num(), e0 + 1);
        assert_eq!(
            dd.cache_stats().invalidations,
            0,
            "caches kept, not emptied"
        );

        // The store sees the header's counts: its own next intern does not
        // reuse the id, and its lazy dictionary is rebuilt to include them.
        assert_eq!(ds.entity_num(), e0 + 1);
        let mine = ds.intern_entity("<http://ex/by-store>").unwrap();
        assert_eq!(mine as usize, e0 + 1, "no id handed out twice");
        assert_eq!(
            dd.lookup(Space::Entity, "<http://ex/by-store>").unwrap(),
            Some(mine)
        );
        let lazy = ds.lazy_dictionary();
        assert_eq!(lazy.term(e).as_deref(), Some("<http://ex/new>"));
        assert_eq!(lazy.term(mine).as_deref(), Some("<http://ex/by-store>"));

        // Durable with the store's commit; an explicit flush keeps the counts.
        ds.flush().unwrap();
        drop((d, dd, lazy, ds));
        let ds = DiskStore::open(&path, 64).unwrap();
        assert_eq!(ds.entity_num(), e0 + 2);
        assert_eq!((ds.literal_num(), ds.predicate_num()), (l0 + 1, p0 + 1));
        let eager = ds.dictionary().unwrap();
        assert_eq!(eager.entity_id("<http://ex/new>"), Some(e));
        assert_eq!(eager.literal_id("\"fresh\""), Some(l));
        assert_eq!(eager.predicate_id("<http://ex/pnew>"), Some(p));
        drop(ds);
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(wal(&path)).ok();
    }

    #[test]
    fn a_failed_intern_leaves_no_trace() {
        let (path, ds) = store("intern_fail", 5);
        let dd = DiskDict::open_shared(ds.shared_pager()).unwrap().writable();
        let before = (dd.counts(), dd.generation());
        // Longer than a B+ tree key may be: the whole operation rolls back
        // (the id2entity slot it wrote first included).
        let huge = format!("<http://ex/{}>", "x".repeat(64 * 1024));
        assert!(dd.intern(Space::Entity, &huge).is_err());
        assert_eq!((dd.counts(), dd.generation()), before);
        assert_eq!(ds.entity_num(), before.0 .0);
        assert_eq!(dd.resolve(Space::Entity, before.0 .0 as u32).unwrap(), None);
        // The next term takes the id the failed one would have had.
        let id = dd.intern(Space::Entity, "<http://ex/ok>").unwrap();
        assert_eq!(id as usize, before.0 .0);
        drop((dd, ds));
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(wal(&path)).ok();
    }
}
