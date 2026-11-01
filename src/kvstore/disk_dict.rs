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
//! * [`prefetch`](DiskTermSource::prefetch) takes ids sorted ascending and
//!   reads them run by run with [`IvArray::for_each_range`]: one directory page
//!   serves 32 consecutive ids and pages are visited in file order.
//!
//! Both caches are bounded by bytes and evict with CLOCK, so a long-running
//! reader stays within its budget however many terms it touches; strings are
//! handed out as `Arc<str>` (no `Box::leak`). They belong to one
//! [`generation`](DiskTermSource::generation): a change of generation empties
//! them. This backend is read-only (`intern` answers existing keys only,
//! `free` refuses); the store that owns the pager rebuilds it when its term counts
//! change, which starts fresh caches.
//!
//! Read errors are returned (`Err`); the [`Dictionary`](crate::dict::Dictionary)
//! front end notes the ones it cannot return in the running statement's read
//! scope (contract C2).

use std::ops::ControlFlow;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::dict::cache::{KeyCache, TermCache, DEFAULT_KEY_CACHE_BYTES, DEFAULT_TERM_CACHE_BYTES};
use crate::dict::{CacheStats, DiskTermSource, Space};
use crate::error::{GStoreError, Result};

use super::bptree::BTree;
use super::format::{
    ENTITY2ID, ID2ENTITY, ID2LITERAL, ID2PREDICATE, LITERAL2ID, PREDICATE2ID, ROOT_ENTITY_COUNT,
    ROOT_LITERAL_COUNT, ROOT_PRED_COUNT,
};
use super::ivarray::{self, IvArray, SLOTS_PER_DIR};
use super::pager::Pager;

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
    entity_count: usize,
    literal_count: usize,
    pred_count: usize,
    terms: TermCache,
    keys: KeyCache,
    /// The cache generation (see the module docs). Constant for this
    /// read-only backend; a writer bumps it whenever an id or key changes
    /// meaning.
    generation: AtomicU64,
}

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
            entity_count,
            literal_count,
            pred_count,
            terms: TermCache::new(DEFAULT_TERM_CACHE_BYTES),
            keys: KeyCache::new(DEFAULT_KEY_CACHE_BYTES),
            generation: AtomicU64::new(0),
        }
    }

    /// A backend over the dictionary structures of the `kvstore.kv` behind
    /// `pager` (e.g. [`DiskStore::shared_pager`](super::DiskStore::shared_pager)),
    /// with the term counts read from its header now.
    pub fn open_shared(pager: Arc<RwLock<Pager>>) -> Result<DiskDict> {
        let counts = {
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
            (
                dense(ROOT_ENTITY_COUNT, "entity")?,
                dense(ROOT_LITERAL_COUNT, "literal")?,
                dense(ROOT_PRED_COUNT, "predicate")?,
            )
        };
        Ok(DiskDict::new(
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
        ))
    }

    /// Replace the caches with empty ones of the given byte budgets
    /// (`id → string` and `string → id`).
    pub fn with_cache_budgets(mut self, term_bytes: usize, key_bytes: usize) -> DiskDict {
        self.terms = TermCache::new(term_bytes);
        self.keys = KeyCache::new(key_bytes);
        self
    }

    fn tree(&self, space: Space) -> &BTree {
        match space {
            Space::Entity => &self.entity2id,
            Space::Literal => &self.literal2id,
            Space::Predicate => &self.predicate2id,
        }
    }

    /// The `id2*` array of `space` and the array key of public id `id`
    /// (`None` for an id outside the space's allocated range).
    fn slot(&self, space: Space, id: u32) -> Option<(&IvArray, u32)> {
        let key = id.checked_sub(space.first_id())?;
        if key as usize >= self.count(space) {
            return None;
        }
        let arr = match space {
            Space::Entity => &self.id2entity,
            Space::Literal => &self.id2literal,
            Space::Predicate => &self.id2predicate,
        };
        Some((arr, key))
    }

    fn pager(&self) -> std::sync::RwLockReadGuard<'_, Pager> {
        // A reader that panicked cannot have left the pager half-written.
        self.pager.read().unwrap_or_else(|e| e.into_inner())
    }

    fn fresh_terms(&self) {
        self.terms
            .check_generation(self.generation.load(Ordering::Acquire));
    }

    fn fresh_keys(&self) {
        self.keys
            .check_generation(self.generation.load(Ordering::Acquire));
    }

    /// Read the strings of `ids` (ascending, within `arr`'s range) run by run,
    /// handing each to `f`.
    fn read_runs(
        &self,
        pager: &Pager,
        arr: &IvArray,
        first: u32,
        keys: &[u32],
        f: &mut dyn FnMut(u32, Arc<str>),
    ) -> Result<()> {
        let mut i = 0;
        while i < keys.len() {
            // A run: keys whose gaps stay within one directory page.
            let mut j = i + 1;
            while j < keys.len() && keys[j] - keys[j - 1] < SLOTS_PER_DIR as u32 {
                j += 1;
            }
            let run = &keys[i..j];
            let mut want = run.iter().peekable();
            arr.for_each_range(pager, run[0], run[run.len() - 1] + 1, |k, v| {
                while want.next_if(|&&w| w < k).is_some() {}
                if want.next_if_eq(&&k).is_some() {
                    f(first + k, decode(v));
                }
                if want.peek().is_none() {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })?;
            i = j;
        }
        Ok(())
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

    /// Read-only: an existing key answers its id, a new one is refused.
    fn intern(&self, space: Space, key: &str) -> Result<u32> {
        self.lookup(space, key)?.ok_or_else(|| {
            GStoreError::Database(format!(
                "this out-of-core dictionary is read-only: cannot intern a new {} term",
                space.name()
            ))
        })
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
            self.read_runs(&pager, arr, first, &keys, &mut |id, s| {
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
        self.read_runs(&pager, arr, first, &missing, &mut |id, s| {
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
        let end = self.count(space) as u32;
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

    fn count(&self, space: Space) -> usize {
        match space {
            Space::Entity => self.entity_count,
            Space::Literal => self.literal_count,
            Space::Predicate => self.pred_count,
        }
    }

    fn id_limit(&self, space: Space) -> u32 {
        space.first_id() + self.count(space) as u32
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
            .field("entity_count", &self.entity_count)
            .field("literal_count", &self.literal_count)
            .field("pred_count", &self.pred_count)
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
}
