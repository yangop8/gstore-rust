//! A disk-backed triple store + dictionary built from B+ trees.
//!
//! Mirrors gStore's `KVstore`: the dictionary trees (`entity2id`/`literal2id`/
//! `predicate2id` and their inverses) plus the triple value indexes. Triples are
//! held in three ordered B+ trees — SPO, POS, OSP — with 12-byte composite keys
//! (big-endian `subject|predicate|object` in each ordering). Prefix range scans
//! over these orderings answer every access pattern, exactly as the in-memory
//! [`crate::store::TripleStore`] does, but entirely from disk through the page
//! cache. Data persists and reopens.

use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use crate::dict::{Dictionary, DiskTermSource};
use crate::error::{GStoreError, Result};
use crate::model::id::{is_literal_id, EntityLiteralId, PredId, LITERAL_FIRST_ID};
use crate::model::{IdTriple, Term, Triple};
use crate::parser::{sparql, turtle};
use crate::query::{Evaluator, QueryResult};
use crate::store::{TripleSource, TripleStore};

use super::bptree::{be32, de32, BTree};
use super::disk_dict::DiskDict;
use super::ivarray::IvArray;
use super::overflow;
use super::pager::{Pager, PAGE_SIZE};
use super::vlist;

// Header root-slot assignment.
const SPO: usize = 0;
const POS: usize = 1;
const OSP: usize = 2;
const ENTITY2ID: usize = 3;
const LITERAL2ID: usize = 4;
const PREDICATE2ID: usize = 5;
const ID2ENTITY: usize = 6;
const ID2LITERAL: usize = 7;
const ID2PREDICATE: usize = 8;
const ROOT_TRIPLE_COUNT: usize = 9;
const ROOT_ENTITY_COUNT: usize = 10;
const ROOT_LITERAL_COUNT: usize = 11;
const ROOT_PRED_COUNT: usize = 12;
/// Root of the optional VList-compressed `(sub, pred) → objects` index.
const SP2O_VLIST: usize = 13;
/// `1` once [`DiskStore::compact`] has built `SP2O_VLIST`; cleared by any write.
const ROOT_COMPACTED: usize = 14;

/// Upper bound on a VList value stored *inline* in a B+tree leaf page. A
/// `(sub, pred)` group whose delta+varint encoding fits within this is stored
/// inline (tag byte + bytes); a larger one spills into an [`overflow`] page
/// chain and the leaf keeps only a small head-pointer record. This is gStore's
/// VList split between in-tree short lists and the separate large-block file.
const MAX_INLINE_VLIST_BYTES: usize = PAGE_SIZE / 2;

/// First byte of a `sp2o_vlist` value: the encoded VList is stored inline,
/// directly following this tag.
const INLINE_TAG: u8 = 0;
/// First byte of a `sp2o_vlist` value: the encoded VList lives in an [`overflow`]
/// page chain. The record is `[OVERFLOW_TAG][head: u32 le][byte_len: u32 le]`.
const OVERFLOW_TAG: u8 = 1;

/// A disk-backed gStore database (dictionary + six-way triple index).
pub struct DiskStore {
    /// `Arc<RwLock<_>>` (not `RefCell`) so the out-of-core [`DiskDict`] can share
    /// the very same pager/page-cache, and so the type stays `Send`-able. The
    /// `RwLock` (task 4) lets many readers resolve pages concurrently — every
    /// read path takes `.read()` and the [`Pager`]'s `read_page(&self)` only
    /// briefly latches its internal cache — while mutations take `.write()`.
    pager: Arc<RwLock<Pager>>,
    spo: BTree,
    pos: BTree,
    osp: BTree,
    entity2id: BTree,
    literal2id: BTree,
    predicate2id: BTree,
    // Reverse (id → string) stores are *integer-keyed*, so they use the dense
    // [`IvArray`] (gStore's IVArray/ISArray) instead of the generic B+ tree.
    id2entity: IvArray,
    id2literal: IvArray,
    id2predicate: IvArray,
    /// Optional VList-compressed `(sub, pred) → objects` index (gStore's value
    /// list). Built by [`compact`](DiskStore::compact); read by `o_by_sp`/`po_by_s`
    /// when [`compacted`](Self::compacted) is set.
    sp2o_vlist: BTree,
    triple_count: u64,
    entity_count: u32,
    literal_count: u32,
    pred_count: u32,
    /// Whether the VList index is current. Set by `compact`, cleared by any write
    /// (so reads always fall back to the authoritative SPO scan when stale).
    compacted: bool,
    /// Lazily built out-of-core dictionary, reused across queries so its
    /// materialized-string cache (and thus its leak) stays bounded by the set of
    /// terms ever touched. Invalidated on any write (counts may have changed).
    /// A `Mutex` (not `RefCell`) so `DiskStore` stays `Sync` and many threads can
    /// share it via `Arc` and read concurrently (task 4).
    disk_dict: Mutex<Option<Arc<DiskDict>>>,
}

impl DiskStore {
    /// Open (or create) a disk store at `path` with a `cache_pages`-page cache.
    pub fn open<P: AsRef<Path>>(path: P, cache_pages: usize) -> Result<DiskStore> {
        let pager = Pager::open(path, cache_pages)?;
        let triple_count = pager.root(ROOT_TRIPLE_COUNT);
        let entity_count = pager.root(ROOT_ENTITY_COUNT) as u32;
        let literal_count = pager.root(ROOT_LITERAL_COUNT) as u32;
        let pred_count = pager.root(ROOT_PRED_COUNT) as u32;
        let compacted = pager.root(ROOT_COMPACTED) != 0;
        Ok(DiskStore {
            pager: Arc::new(RwLock::new(pager)),
            spo: BTree::new(SPO),
            pos: BTree::new(POS),
            osp: BTree::new(OSP),
            entity2id: BTree::new(ENTITY2ID),
            literal2id: BTree::new(LITERAL2ID),
            predicate2id: BTree::new(PREDICATE2ID),
            id2entity: IvArray::new(ID2ENTITY),
            id2literal: IvArray::new(ID2LITERAL),
            id2predicate: IvArray::new(ID2PREDICATE),
            sp2o_vlist: BTree::new(SP2O_VLIST),
            triple_count,
            entity_count,
            literal_count,
            pred_count,
            compacted,
            disk_dict: Mutex::new(None),
        })
    }

    /// Build a disk store from RDF (Turtle/N-Triples) files.
    pub fn build_files<P1: AsRef<Path>, P2: AsRef<Path>>(
        path: P1,
        cache_pages: usize,
        rdf_files: &[P2],
    ) -> Result<DiskStore> {
        let mut store = DiskStore::open(path, cache_pages)?;
        for f in rdf_files {
            for t in turtle::parse_file(f)? {
                store.insert_triple(&t)?;
            }
        }
        store.flush()?;
        Ok(store)
    }

    /// Build a disk store from an in-memory RDF document (handy for tests).
    pub fn build_str<P: AsRef<Path>>(
        path: P,
        cache_pages: usize,
        content: &str,
    ) -> Result<DiskStore> {
        let mut store = DiskStore::open(path, cache_pages)?;
        for t in turtle::parse_str(content)? {
            store.insert_triple(&t)?;
        }
        store.flush()?;
        Ok(store)
    }

    // ---- building ---------------------------------------------------------

    fn intern_entity(&mut self, key: &str) -> Result<EntityLiteralId> {
        let mut guard = self.pager.write().unwrap();
        let pager = &mut *guard;
        if let Some(v) = self.entity2id.get(pager, key.as_bytes())? {
            return Ok(de32(&v));
        }
        let id = self.entity_count;
        self.entity2id.insert(pager, key.as_bytes(), &be32(id))?;
        // id2entity is integer-keyed by the dense entity id.
        self.id2entity.insert(pager, id, key.as_bytes())?;
        self.entity_count += 1;
        // Keep the count root current so any pager flush (incl. eviction) is
        // crash-consistent with the dictionary trees.
        pager.set_root(ROOT_ENTITY_COUNT, self.entity_count as u64);
        Ok(id)
    }

    fn intern_literal(&mut self, key: &str) -> Result<EntityLiteralId> {
        let mut guard = self.pager.write().unwrap();
        let pager = &mut *guard;
        if let Some(v) = self.literal2id.get(pager, key.as_bytes())? {
            return Ok(de32(&v));
        }
        let id = LITERAL_FIRST_ID
            .checked_add(self.literal_count)
            .ok_or_else(|| GStoreError::Database("literal id space exhausted".to_string()))?;
        self.literal2id.insert(pager, key.as_bytes(), &be32(id))?;
        // id2literal is integer-keyed by the *local* literal index (id minus the
        // LITERAL_FIRST_ID offset) so the dense array stays compact from 0.
        self.id2literal.insert(pager, id - LITERAL_FIRST_ID, key.as_bytes())?;
        self.literal_count += 1;
        pager.set_root(ROOT_LITERAL_COUNT, self.literal_count as u64);
        Ok(id)
    }

    fn intern_predicate(&mut self, key: &str) -> Result<PredId> {
        let mut guard = self.pager.write().unwrap();
        let pager = &mut *guard;
        if let Some(v) = self.predicate2id.get(pager, key.as_bytes())? {
            return Ok(de32(&v));
        }
        let id = self.pred_count;
        self.predicate2id.insert(pager, key.as_bytes(), &be32(id))?;
        // id2predicate is integer-keyed by the dense predicate id.
        self.id2predicate.insert(pager, id, key.as_bytes())?;
        self.pred_count += 1;
        pager.set_root(ROOT_PRED_COUNT, self.pred_count as u64);
        Ok(id)
    }

    fn intern_term(&mut self, t: &Term) -> Result<EntityLiteralId> {
        let key = t.dict_key();
        if t.is_literal() {
            self.intern_literal(&key)
        } else {
            self.intern_entity(&key)
        }
    }

    /// Insert one triple; returns `true` if newly added.
    pub fn insert_triple(&mut self, t: &Triple) -> Result<bool> {
        let s = self.intern_entity(&t.subject.dict_key())?;
        let p = self.intern_predicate(&t.predicate.dict_key())?;
        let o = self.intern_term(&t.object)?;
        self.insert_ids(IdTriple::new(s, p, o))
    }

    // Crash-consistency scope: the WAL makes each *pager flush* atomic, and the
    // count roots above are kept current so they never lag the trees. But a
    // logical triple is three B+ tree inserts (SPO/POS/OSP); under cache pressure
    // an eviction-triggered flush can land between them, so a crash mid-insert
    // can leave the indexes disagreeing for that one triple. Full per-operation
    // atomicity (a single WAL batch per triple / page pinning) is future work.
    fn insert_ids(&mut self, t: IdTriple) -> Result<bool> {
        let mut guard = self.pager.write().unwrap();
        let pager = &mut *guard;
        let spo = key3(t.sub, t.pred, t.obj);
        if self.spo.get(pager, &spo)?.is_some() {
            return Ok(false);
        }
        self.spo.insert(pager, &spo, b"")?;
        self.pos.insert(pager, &key3(t.pred, t.obj, t.sub), b"")?;
        self.osp.insert(pager, &key3(t.obj, t.sub, t.pred), b"")?;
        self.triple_count += 1;
        pager.set_root(ROOT_TRIPLE_COUNT, self.triple_count);
        if self.compacted {
            // The VList index no longer reflects the data; reads must rescan.
            self.compacted = false;
            pager.set_root(ROOT_COMPACTED, 0);
        }
        Ok(true)
    }

    /// Delete one triple from all three indexes (gStore `KVstore::removeTriple`).
    /// Returns `true` if it existed. Dictionary entries are intentionally kept
    /// (gStore does not reclaim string ids on triple deletion); only the triple
    /// indexes shrink, with the B+ trees merging underfull nodes and freeing
    /// pages. Returns `false` (no change) if any term is unknown or the triple
    /// is absent.
    pub fn delete_triple(&mut self, t: &Triple) -> Result<bool> {
        let s = {
            let guard = self.pager.read().unwrap();
            let pager = &*guard;
            match self.entity2id.get(pager, t.subject.dict_key().as_bytes())? {
                Some(v) => de32(&v),
                None => return Ok(false),
            }
        };
        let p = {
            let guard = self.pager.read().unwrap();
            let pager = &*guard;
            match self.predicate2id.get(pager, t.predicate.dict_key().as_bytes())? {
                Some(v) => de32(&v),
                None => return Ok(false),
            }
        };
        let o = {
            let guard = self.pager.read().unwrap();
            let pager = &*guard;
            let tree = if t.object.is_literal() {
                &self.literal2id
            } else {
                &self.entity2id
            };
            match tree.get(pager, t.object.dict_key().as_bytes())? {
                Some(v) => de32(&v),
                None => return Ok(false),
            }
        };
        self.delete_ids(IdTriple::new(s, p, o))
    }

    fn delete_ids(&mut self, t: IdTriple) -> Result<bool> {
        let mut guard = self.pager.write().unwrap();
        let pager = &mut *guard;
        let spo = key3(t.sub, t.pred, t.obj);
        if self.spo.get(pager, &spo)?.is_none() {
            return Ok(false);
        }
        self.spo.delete(pager, &spo)?;
        self.pos.delete(pager, &key3(t.pred, t.obj, t.sub))?;
        self.osp.delete(pager, &key3(t.obj, t.sub, t.pred))?;
        self.triple_count -= 1;
        pager.set_root(ROOT_TRIPLE_COUNT, self.triple_count);
        if self.compacted {
            self.compacted = false;
            pager.set_root(ROOT_COMPACTED, 0);
        }
        Ok(true)
    }

    /// Persist counters and flush all dirty pages.
    pub fn flush(&mut self) -> Result<()> {
        let mut guard = self.pager.write().unwrap();
        let pager = &mut *guard;
        pager.set_root(ROOT_TRIPLE_COUNT, self.triple_count);
        pager.set_root(ROOT_ENTITY_COUNT, self.entity_count as u64);
        pager.set_root(ROOT_LITERAL_COUNT, self.literal_count as u64);
        pager.set_root(ROOT_PRED_COUNT, self.pred_count as u64);
        pager.flush()
    }

    // ---- counts -----------------------------------------------------------

    pub fn triple_count(&self) -> u64 {
        self.triple_count
    }
    pub fn entity_num(&self) -> usize {
        self.entity_count as usize
    }
    pub fn literal_num(&self) -> usize {
        self.literal_count as usize
    }
    pub fn predicate_num(&self) -> usize {
        self.pred_count as usize
    }

    // ---- dictionary resolution -------------------------------------------

    pub fn term_id(&self, t: &Term) -> Result<Option<EntityLiteralId>> {
        let key = t.dict_key();
        let pager = self.pager.read().unwrap();
        let tree = if t.is_literal() {
            &self.literal2id
        } else {
            &self.entity2id
        };
        Ok(tree.get(&pager, key.as_bytes())?.map(|v| de32(&v)))
    }

    pub fn predicate_id(&self, dict_key: &str) -> Result<Option<PredId>> {
        let pager = self.pager.read().unwrap();
        Ok(self
            .predicate2id
            .get(&pager, dict_key.as_bytes())?
            .map(|v| de32(&v)))
    }

    pub fn id_to_string(&self, id: EntityLiteralId) -> Result<Option<String>> {
        let pager = self.pager.read().unwrap();
        // Literals are keyed by their local index (see `intern_literal`).
        let (arr, key) = if is_literal_id(id) {
            (&self.id2literal, id - LITERAL_FIRST_ID)
        } else {
            (&self.id2entity, id)
        };
        Ok(arr
            .get(&pager, key)?
            .map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    pub fn predicate_to_string(&self, id: PredId) -> Result<Option<String>> {
        let pager = self.pager.read().unwrap();
        Ok(self
            .id2predicate
            .get(&pager, id)?
            .map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    // ---- access patterns (mirror TripleStore) ----------------------------

    pub fn exists(&self, s: EntityLiteralId, p: PredId, o: EntityLiteralId) -> Result<bool> {
        let pager = self.pager.read().unwrap();
        Ok(self.spo.get(&pager, &key3(s, p, o))?.is_some())
    }

    /// `s ? ?` → `(pred, obj)` pairs (sorted by (pred, obj)).
    pub fn po_by_s(&self, s: EntityLiteralId) -> Result<Vec<(PredId, EntityLiteralId)>> {
        let rows = self.scan(&self.spo, &be32(s))?;
        Ok(rows
            .iter()
            .map(|k| (de32(&k[4..8]), de32(&k[8..12])))
            .collect())
    }

    /// `s p ?` → objects. When the store has been [`compact`](Self::compact)ed,
    /// this is a point lookup into the VList index + a varint decode (following
    /// an [`overflow`] page chain for an oversize group), instead of a multi-key
    /// prefix scan; it transparently falls back to the SPO scan for any group
    /// not present in the index.
    pub fn o_by_sp(&self, s: EntityLiteralId, p: PredId) -> Result<Vec<EntityLiteralId>> {
        if self.compacted {
            let pager = self.pager.read().unwrap();
            if let Some(rec) = self.sp2o_vlist.get(&pager, &cat(s, p))? {
                let bytes = materialize_vlist(&pager, &rec)?;
                return Ok(vlist::decode_u32s(&bytes).unwrap_or_default());
            }
        }
        let rows = self.scan(&self.spo, &cat(s, p))?;
        Ok(rows.iter().map(|k| de32(&k[8..12])).collect())
    }

    /// `? p o` → subjects.
    pub fn s_by_po(&self, p: PredId, o: EntityLiteralId) -> Result<Vec<EntityLiteralId>> {
        let rows = self.scan(&self.pos, &cat(p, o))?;
        Ok(rows.iter().map(|k| de32(&k[8..12])).collect())
    }

    /// `? p ?` → `(sub, obj)` pairs.
    pub fn so_by_p(&self, p: PredId) -> Result<Vec<(EntityLiteralId, EntityLiteralId)>> {
        // POS key = (p, o, s); map to (s, o).
        let rows = self.scan(&self.pos, &be32(p))?;
        Ok(rows
            .iter()
            .map(|k| (de32(&k[8..12]), de32(&k[4..8])))
            .collect())
    }

    /// `? ? o` → `(pred, sub)` pairs.
    pub fn ps_by_o(&self, o: EntityLiteralId) -> Result<Vec<(PredId, EntityLiteralId)>> {
        // OSP key = (o, s, p); map to (p, s).
        let rows = self.scan(&self.osp, &be32(o))?;
        Ok(rows
            .iter()
            .map(|k| (de32(&k[8..12]), de32(&k[4..8])))
            .collect())
    }

    /// `s ? o` → predicates linking subject to object.
    pub fn p_by_so(&self, s: EntityLiteralId, o: EntityLiteralId) -> Result<Vec<PredId>> {
        // OSP key = (o, s, p); prefix (o, s) → p.
        let rows = self.scan(&self.osp, &cat(o, s))?;
        Ok(rows.iter().map(|k| de32(&k[8..12])).collect())
    }

    /// Iterate every triple (driven by the SPO index).
    pub fn iter_all(&self) -> Result<Vec<IdTriple>> {
        let rows = self.scan(&self.spo, &[])?;
        Ok(rows
            .iter()
            .map(|k| IdTriple::new(de32(&k[0..4]), de32(&k[4..8]), de32(&k[8..12])))
            .collect())
    }

    fn scan(&self, tree: &BTree, prefix: &[u8]) -> Result<Vec<Vec<u8>>> {
        let pager = self.pager.read().unwrap();
        Ok(tree
            .scan_prefix(&pager, prefix)?
            .into_iter()
            .map(|(k, _)| k)
            .collect())
    }

    // ---- VList value compaction (gStore value lists) ----------------------

    /// Build (or rebuild) the VList-compressed `(sub, pred) → objects` index from
    /// the authoritative SPO index, then mark the store compacted so `o_by_sp`
    /// reads it. Each group's sorted object list is delta+varint encoded (see
    /// [`vlist`](super::vlist)), trading many tiny per-triple keys for one compact
    /// value per `(sub, pred)`. A group whose encoding fits in
    /// [`MAX_INLINE_VLIST_BYTES`] is stored inline; a larger one spills into an
    /// [`overflow`] page chain (gStore's VList large-block file), so arbitrarily
    /// long posting lists — e.g. a hub subject or a dense predicate — compress and
    /// round-trip on disk rather than being skipped.
    ///
    /// Any subsequent insert/delete clears the compacted flag, so the index is a
    /// read-side optimization that never returns stale data.
    pub fn compact(&mut self) -> Result<()> {
        // 0. Mark the index stale up-front: if a crash interrupts the rebuild
        //    below, the next open falls back to the authoritative SPO scan.
        {
            let mut guard = self.pager.write().unwrap();
            self.compacted = false;
            guard.set_root(ROOT_COMPACTED, 0);
        }

        // 1. Drop any previous VList entries so vanished groups can't be read,
        //    freeing each overflow chain so its pages return to the free list.
        let stale: Vec<(Vec<u8>, Vec<u8>)> = {
            let guard = self.pager.read().unwrap();
            self.sp2o_vlist.iter_all(&guard)?
        };
        for (k, v) in &stale {
            let mut guard = self.pager.write().unwrap();
            if let Some(head) = overflow_head(v) {
                overflow::free_chain(&mut guard, head)?;
            }
            self.sp2o_vlist.delete(&mut guard, k)?;
        }

        // 2. Group the SPO index by (sub, pred); each group's objects are already
        //    sorted ascending and distinct (SPO key order), ready for encoding.
        let all = self.scan(&self.spo, &[])?;
        let mut i = 0;
        while i < all.len() {
            let s = de32(&all[i][0..4]);
            let p = de32(&all[i][4..8]);
            let mut objs = Vec::new();
            while i < all.len() && de32(&all[i][0..4]) == s && de32(&all[i][4..8]) == p {
                objs.push(de32(&all[i][8..12]));
                i += 1;
            }
            let enc = vlist::encode_u32s(&objs);
            let mut guard = self.pager.write().unwrap();
            let rec = encode_vlist_record(&mut guard, &enc)?;
            self.sp2o_vlist.insert(&mut guard, &cat(s, p), &rec)?;
        }

        // 3. Publish: reads may now use the VList index.
        let mut guard = self.pager.write().unwrap();
        self.compacted = true;
        guard.set_root(ROOT_COMPACTED, 1);
        Ok(())
    }

    /// Whether the VList-compressed index is currently valid (set by
    /// [`compact`](Self::compact), cleared by any write).
    pub fn is_compacted(&self) -> bool {
        self.compacted
    }

    /// Total allocated page count (for tests asserting no page leaks).
    #[cfg(test)]
    fn page_count_for_test(&self) -> u32 {
        self.pager.read().unwrap().page_count()
    }

    /// Logical (delta+varint) byte length of the stored VList value for `(s, p)`
    /// in the compressed index, or `None` if absent (not compacted / empty
    /// group). This is the encoded payload size whether the value is stored
    /// inline or in an [`overflow`] chain, so size/round-trip assertions are
    /// independent of where the bytes physically live.
    pub fn compact_value_len(&self, s: EntityLiteralId, p: PredId) -> Result<Option<usize>> {
        let pager = self.pager.read().unwrap();
        Ok(self
            .sp2o_vlist
            .get(&pager, &cat(s, p))?
            .map(|v| record_logical_len(&v)))
    }

    /// Whether the stored VList value for `(s, p)` uses an [`overflow`] page
    /// chain (i.e. its encoding exceeded [`MAX_INLINE_VLIST_BYTES`]). `None` if
    /// the group is absent from the compressed index. Exposes the inline/overflow
    /// decision for tests.
    pub fn compact_value_is_overflow(&self, s: EntityLiteralId, p: PredId) -> Result<Option<bool>> {
        let pager = self.pager.read().unwrap();
        Ok(self
            .sp2o_vlist
            .get(&pager, &cat(s, p))?
            .map(|v| v.first() == Some(&OVERFLOW_TAG)))
    }

    // ---- bridge to the in-memory engine ----------------------------------

    /// Reconstruct an in-memory [`Dictionary`] + [`TripleStore`] from disk.
    /// Ids are reassigned in the same order, so the reconstructed state is
    /// identical to having built in memory — allowing the existing query engine
    /// (and VS-tree) to run against a disk-built database.
    pub fn to_memory(&self) -> Result<(Dictionary, TripleStore)> {
        let dict = self.dictionary()?;
        let mut store = TripleStore::new();
        store.bulk_load(self.iter_all()?);
        Ok((dict, store))
    }

    /// Reconstruct just the in-memory [`Dictionary`] (ids in disk order, so they
    /// line up with the on-disk triple indexes). Used by [`query`](Self::query)
    /// to answer reads while leaving the triples on disk.
    pub fn dictionary(&self) -> Result<Dictionary> {
        let mut dict = Dictionary::new();
        for id in 0..self.entity_count {
            if let Some(s) = self.id_to_string(id)? {
                dict.intern_entity(&s);
            }
        }
        for i in 0..self.literal_count {
            let lit_id = LITERAL_FIRST_ID
                .checked_add(i)
                .ok_or_else(|| GStoreError::Database("literal id space exhausted".to_string()))?;
            if let Some(s) = self.id_to_string(lit_id)? {
                dict.intern_literal(&s);
            }
        }
        for id in 0..self.pred_count {
            if let Some(s) = self.predicate_to_string(id)? {
                dict.intern_predicate(&s);
            }
        }
        Ok(dict)
    }

    /// Get (building once, then reusing) the out-of-core dictionary backend that
    /// shares this store's pager/page-cache. Rebuilt only if the term counts have
    /// changed since it was last built (i.e. new terms were interned), so reads
    /// keep reusing one materialized-string cache.
    fn disk_backing(&self) -> Arc<DiskDict> {
        let mut cached = self.disk_dict.lock().unwrap();
        if let Some(d) = cached.as_ref() {
            if d.entity_num() == self.entity_count as usize
                && d.literal_num() == self.literal_count as usize
                && d.predicate_num() == self.pred_count as usize
            {
                return Arc::clone(d);
            }
        }
        let d = Arc::new(DiskDict::new(
            Arc::clone(&self.pager),
            self.entity2id,
            self.literal2id,
            self.predicate2id,
            self.id2entity,
            self.id2literal,
            self.id2predicate,
            self.entity_count as usize,
            self.literal_count as usize,
            self.pred_count as usize,
        ));
        *cached = Some(Arc::clone(&d));
        d
    }

    /// An *out-of-core* [`Dictionary`] that resolves str↔id from the on-disk
    /// B+trees on demand (see [`DiskDict`]). Unlike [`dictionary`](Self::dictionary),
    /// which eagerly loads every string into RAM, this materializes only the terms
    /// actually looked up — letting a dictionary larger than RAM back a query.
    pub fn lazy_dictionary(&self) -> Dictionary {
        Dictionary::from_backing(self.disk_backing())
    }

    /// Number of dictionary strings currently resident in RAM via the lazy
    /// [`lazy_dictionary`](Self::lazy_dictionary) path (0 before any such query).
    /// A small value relative to [`entity_num`](Self::entity_num) proves the
    /// dictionary was *not* fully loaded.
    pub fn resident_string_count(&self) -> usize {
        self.disk_dict
            .lock()
            .unwrap()
            .as_ref()
            .map_or(0, |d| d.resident_string_count())
    }

    /// Answer a SPARQL read query (SELECT/ASK/CONSTRUCT/DESCRIBE) by *streaming*
    /// matches directly from the on-disk indexes. Both the triple indexes **and**
    /// the dictionary are read on demand through the page cache: term→id lookups
    /// for query constants hit the dictionary B+trees per call, and id→str
    /// materialization of result rows fetches strings lazily (see
    /// [`lazy_dictionary`](Self::lazy_dictionary)). So a database whose dictionary
    /// alone exceeds RAM can still be queried without a full load (the
    /// [`to_memory`](Self::to_memory) path does materialize). Updates and the
    /// VS-tree filter are not available on this read-only streaming path.
    pub fn query(&self, sparql: &str) -> Result<QueryResult> {
        let dict = self.lazy_dictionary();
        let q = sparql::parse(sparql)?;
        Evaluator::new(&dict, self).evaluate(&q)
    }
}

/// Distinct first / second components of `(a, b)` pairs.
fn distinct_a(pairs: &[(u32, u32)]) -> Vec<u32> {
    let mut v: Vec<u32> = pairs.iter().map(|&(a, _)| a).collect();
    v.sort_unstable();
    v.dedup();
    v
}
fn distinct_b(pairs: &[(u32, u32)]) -> Vec<u32> {
    let mut v: Vec<u32> = pairs.iter().map(|&(_, b)| b).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Let the query engine evaluate directly against the on-disk store, *streaming*
/// the index ranges each pattern touches through the page cache rather than
/// materializing the whole graph in memory. IO errors degrade to empty results
/// (a read-only path; a healthy store never hits them). Cardinality stats use
/// the O(1) dictionary counters as estimates, avoiding full scans on the hot
/// optimizer path.
impl TripleSource for DiskStore {
    fn exists(&self, s: u32, p: u32, o: u32) -> bool {
        DiskStore::exists(self, s, p, o).unwrap_or(false)
    }
    fn po_by_s(&self, s: u32) -> Vec<(u32, u32)> {
        DiskStore::po_by_s(self, s).unwrap_or_default()
    }
    fn o_by_sp(&self, s: u32, p: u32) -> Vec<u32> {
        DiskStore::o_by_sp(self, s, p).unwrap_or_default()
    }
    fn p_by_so(&self, s: u32, o: u32) -> Vec<u32> {
        DiskStore::p_by_so(self, s, o).unwrap_or_default()
    }
    fn ps_by_o(&self, o: u32) -> Vec<(u32, u32)> {
        DiskStore::ps_by_o(self, o).unwrap_or_default()
    }
    fn s_by_po(&self, p: u32, o: u32) -> Vec<u32> {
        DiskStore::s_by_po(self, p, o).unwrap_or_default()
    }
    fn so_by_p(&self, p: u32) -> Vec<(u32, u32)> {
        DiskStore::so_by_p(self, p).unwrap_or_default()
    }
    fn subs_by_p(&self, p: u32) -> Vec<u32> {
        distinct_a(&DiskStore::so_by_p(self, p).unwrap_or_default())
    }
    fn objs_by_p(&self, p: u32) -> Vec<u32> {
        distinct_b(&DiskStore::so_by_p(self, p).unwrap_or_default())
    }
    fn subject_keys(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.iter_all().unwrap_or_default().iter().map(|t| t.sub).collect();
        v.sort_unstable();
        v.dedup();
        v
    }
    fn object_keys(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.iter_all().unwrap_or_default().iter().map(|t| t.obj).collect();
        v.sort_unstable();
        v.dedup();
        v
    }
    fn triple_count(&self) -> u64 {
        DiskStore::triple_count(self)
    }
    // Counter-based estimates (upper bounds) keep the cost model O(1) on disk.
    fn distinct_subjects(&self) -> usize {
        self.entity_num().max(1)
    }
    fn distinct_objects(&self) -> usize {
        self.entity_num() + self.literal_num()
    }
    fn num_predicates(&self) -> usize {
        self.predicate_num()
    }
    fn pred_card(&self, p: u32) -> usize {
        DiskStore::so_by_p(self, p).unwrap_or_default().len()
    }
    fn pred_distinct_subj(&self, p: u32) -> usize {
        distinct_a(&DiskStore::so_by_p(self, p).unwrap_or_default()).len()
    }
    fn pred_distinct_obj(&self, p: u32) -> usize {
        distinct_b(&DiskStore::so_by_p(self, p).unwrap_or_default()).len()
    }
    fn iter_all(&self) -> Vec<IdTriple> {
        DiskStore::iter_all(self).unwrap_or_default()
    }
}

/// Build a `sp2o_vlist` value record for an encoded VList: inline (tag + bytes)
/// when small enough, else an [`overflow`] chain written here, leaving only a
/// `[OVERFLOW_TAG][head][byte_len]` head-pointer record in the tree.
fn encode_vlist_record(pager: &mut Pager, enc: &[u8]) -> Result<Vec<u8>> {
    if enc.len() <= MAX_INLINE_VLIST_BYTES {
        let mut rec = Vec::with_capacity(1 + enc.len());
        rec.push(INLINE_TAG);
        rec.extend_from_slice(enc);
        Ok(rec)
    } else {
        let head = overflow::write_chain(pager, enc)?;
        let mut rec = Vec::with_capacity(9);
        rec.push(OVERFLOW_TAG);
        rec.extend_from_slice(&head.to_le_bytes());
        rec.extend_from_slice(&(enc.len() as u32).to_le_bytes());
        Ok(rec)
    }
}

/// Reassemble the encoded VList bytes from a `sp2o_vlist` value record, reading
/// an [`overflow`] chain when the record is a head pointer. A malformed record
/// yields empty bytes (decodes to an empty list).
fn materialize_vlist(pager: &Pager, rec: &[u8]) -> Result<Vec<u8>> {
    match rec.first() {
        Some(&INLINE_TAG) => Ok(rec[1..].to_vec()),
        Some(&OVERFLOW_TAG) if rec.len() >= 9 => {
            let head = u32::from_le_bytes(rec[1..5].try_into().unwrap());
            let len = u32::from_le_bytes(rec[5..9].try_into().unwrap()) as usize;
            overflow::read_chain(pager, head, len)
        }
        _ => Ok(Vec::new()),
    }
}

/// Logical (encoded) byte length carried by a `sp2o_vlist` value record,
/// regardless of inline vs overflow storage.
fn record_logical_len(rec: &[u8]) -> usize {
    match rec.first() {
        Some(&INLINE_TAG) => rec.len() - 1,
        Some(&OVERFLOW_TAG) if rec.len() >= 9 => {
            u32::from_le_bytes(rec[5..9].try_into().unwrap()) as usize
        }
        _ => 0,
    }
}

/// The overflow head page id of a `sp2o_vlist` value record, or `None` if the
/// record is inline (no chain to free).
fn overflow_head(rec: &[u8]) -> Option<super::pager::PageId> {
    if rec.first() == Some(&OVERFLOW_TAG) && rec.len() >= 5 {
        Some(u32::from_le_bytes(rec[1..5].try_into().unwrap()))
    } else {
        None
    }
}

/// 12-byte composite key from three ids (big-endian, ordering-preserving).
fn key3(a: u32, b: u32, c: u32) -> [u8; 12] {
    let mut k = [0u8; 12];
    k[0..4].copy_from_slice(&be32(a));
    k[4..8].copy_from_slice(&be32(b));
    k[8..12].copy_from_slice(&be32(c));
    k
}

/// 8-byte two-id prefix.
fn cat(a: u32, b: u32) -> [u8; 8] {
    let mut k = [0u8; 8];
    k[0..4].copy_from_slice(&be32(a));
    k[4..8].copy_from_slice(&be32(b));
    k
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMALL: &str = "\
<root> <name> \"Bookug Lobert\" .
<root> <contain> <node0> .
<root> <contain> <node1> .
<node1> <own> <point0> .
<node1> <own> <point1> .
";

    fn tmp(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("gstore_disk_{tag}.kv"));
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(p.with_extension("kv.wal"));
        p
    }

    #[test]
    fn build_counts_match() {
        let path = tmp("counts");
        let ds = DiskStore::build_str(&path, 64, SMALL).unwrap();
        assert_eq!(ds.triple_count(), 5);
        assert_eq!(ds.entity_num(), 5); // root, node0, node1, point0, point1
        assert_eq!(ds.literal_num(), 1);
        assert_eq!(ds.predicate_num(), 3);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn access_patterns_against_memory() {
        // The disk store must answer every access pattern identically to the
        // in-memory store built from the same data.
        let path = tmp("patterns");
        let ds = DiskStore::build_str(&path, 64, SMALL).unwrap();
        let (dict, mem) = ds.to_memory().unwrap();

        let root = dict.entity_id(&Term::iri("root").dict_key()).unwrap();
        let contain = dict.predicate_id(&Term::iri("contain").dict_key()).unwrap();

        // s p ?
        let mut disk = ds.o_by_sp(root, contain).unwrap();
        disk.sort_unstable();
        let mut memv = mem.o_by_sp(root, contain);
        memv.sort_unstable();
        assert_eq!(disk, memv);

        // s ? ?
        let mut dp = ds.po_by_s(root).unwrap();
        dp.sort_unstable();
        let mut mp = mem.po_by_s(root).to_vec();
        mp.sort_unstable();
        assert_eq!(dp, mp);

        // exists
        let node0 = dict.entity_id(&Term::iri("node0").dict_key()).unwrap();
        assert!(ds.exists(root, contain, node0).unwrap());
        assert!(!ds.exists(node0, contain, root).unwrap());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn persists_and_reopens() {
        let path = tmp("reopen");
        {
            let _ = DiskStore::build_str(&path, 64, SMALL).unwrap();
        }
        let ds = DiskStore::open(&path, 64).unwrap();
        assert_eq!(ds.triple_count(), 5);
        // A query-shaped lookup works after reopen.
        let (dict, _mem) = ds.to_memory().unwrap();
        let node1 = dict.entity_id(&Term::iri("node1").dict_key()).unwrap();
        let own = dict.predicate_id(&Term::iri("own").dict_key()).unwrap();
        assert_eq!(ds.o_by_sp(node1, own).unwrap().len(), 2);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn iter_all_roundtrips() {
        let path = tmp("iter");
        let ds = DiskStore::build_str(&path, 64, SMALL).unwrap();
        assert_eq!(ds.iter_all().unwrap().len(), 5);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn delete_triple_updates_all_indexes() {
        let path = tmp("del_triple");
        let mut ds = DiskStore::build_str(&path, 64, SMALL).unwrap();
        let t = Triple::new(Term::iri("node1"), Term::iri("own"), Term::iri("point0"));
        assert!(ds.delete_triple(&t).unwrap());
        assert_eq!(ds.triple_count(), 4);
        // deleting again is a no-op
        assert!(!ds.delete_triple(&t).unwrap());

        // the triple is gone from every access pattern
        let node1 = {
            let (dict, _) = ds.to_memory().unwrap();
            dict.entity_id(&Term::iri("node1").dict_key()).unwrap()
        };
        let own = ds.predicate_id(&Term::iri("own").dict_key()).unwrap().unwrap();
        let point0 = {
            let (dict, _) = ds.to_memory().unwrap();
            dict.entity_id(&Term::iri("point0").dict_key()).unwrap()
        };
        assert!(!ds.exists(node1, own, point0).unwrap());
        let objs = ds.o_by_sp(node1, own).unwrap();
        assert_eq!(objs.len(), 1, "only point1 remains under (node1, own)");
        assert!(!objs.contains(&point0));
        assert!(!ds.s_by_po(own, point0).unwrap().contains(&node1));
        assert!(!ds.p_by_so(node1, point0).unwrap().contains(&own));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn delete_then_reopen_persists() {
        let path = tmp("del_reopen");
        {
            let mut ds = DiskStore::build_str(&path, 64, SMALL).unwrap();
            let t = Triple::new(Term::iri("root"), Term::iri("contain"), Term::iri("node0"));
            assert!(ds.delete_triple(&t).unwrap());
            ds.flush().unwrap();
        }
        let ds = DiskStore::open(&path, 64).unwrap();
        assert_eq!(ds.triple_count(), 4);
        assert_eq!(ds.iter_all().unwrap().len(), 4);
        std::fs::remove_file(&path).ok();
    }

    // ---- out-of-core dictionary (lazy str↔id resolution) ------------------

    #[test]
    fn lazy_dictionary_resolves_term_and_id_on_demand() {
        let path = tmp("lazy_dict_direct");
        let ds = DiskStore::build_str(&path, 64, SMALL).unwrap();
        let dict = ds.lazy_dictionary();
        assert!(dict.is_disk_backed());

        // term → id goes straight to the B+tree and caches no string.
        let node1 = dict.entity_id(&Term::iri("node1").dict_key()).unwrap();
        let own = dict.predicate_id(&Term::iri("own").dict_key()).unwrap();
        assert_eq!(ds.resident_string_count(), 0, "term→id must not materialize");

        // id → str materializes exactly the looked-up strings.
        assert_eq!(dict.id_to_string(node1), Some("<node1>"));
        assert_eq!(dict.predicate_to_string(own), Some("<own>"));
        assert_eq!(ds.resident_string_count(), 2);

        // Re-resolving an already-seen id is free (no extra residency).
        assert_eq!(dict.id_to_string(node1), Some("<node1>"));
        assert_eq!(ds.resident_string_count(), 2);

        // Unknown terms/ids resolve to None without panicking.
        assert_eq!(dict.entity_id(&Term::iri("ghost").dict_key()), None);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn lazy_query_does_not_load_whole_dictionary() {
        // A query over a disk store must answer correctly while keeping only the
        // terms it actually touches resident — not the full dictionary.
        let path = tmp("lazy_ooc");
        let n = 300u32;
        let mut content = String::new();
        for i in 0..n {
            content.push_str(&format!("<http://ex/s{i}> <http://ex/p> <http://ex/o{i}> .\n"));
        }
        let ds = DiskStore::build_str(&path, 64, &content).unwrap();
        // s0..s(n-1) and o0..o(n-1) are distinct entities; p is a predicate.
        assert_eq!(ds.entity_num(), 2 * n as usize);
        assert_eq!(ds.resident_string_count(), 0, "nothing resident before a query");

        let res = ds
            .query("SELECT ?o WHERE { <http://ex/s5> <http://ex/p> ?o }")
            .unwrap();
        match res {
            QueryResult::Select(rs) => {
                assert_eq!(rs.row_count(), 1);
                assert_eq!(rs.rows[0][0].as_deref(), Some("<http://ex/o5>"));
            }
            other => panic!("expected SELECT, got {other:?}"),
        }

        // Only the projected object's string was materialized — a tiny fraction
        // of the 600-entity dictionary.
        let resident = ds.resident_string_count();
        assert!(resident >= 1, "the result row must materialize its string");
        assert!(
            resident <= 4,
            "out-of-core: only looked-up keys resident, got {resident} of {}",
            ds.entity_num()
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn lazy_query_matches_eager_results() {
        // The lazy (out-of-core) dictionary path must produce the same answers as
        // resolving against a fully-materialized in-memory dictionary.
        let path = tmp("lazy_vs_eager");
        let n = 50u32;
        let mut content = String::new();
        for i in 0..n {
            content.push_str(&format!("<http://ex/s{i}> <http://ex/p> <http://ex/o{i}> .\n"));
        }
        let ds = DiskStore::build_str(&path, 64, &content).unwrap();
        let sparql = "SELECT ?s ?o WHERE { ?s <http://ex/p> ?o }";

        // Lazy path (DiskStore::query, on-demand dictionary).
        let lazy = match ds.query(sparql).unwrap() {
            QueryResult::Select(rs) => rs,
            other => panic!("expected SELECT, got {other:?}"),
        };
        // Eager path (full in-memory dictionary + store).
        let (dict, mem) = ds.to_memory().unwrap();
        let q = sparql::parse(sparql).unwrap();
        let eager = match Evaluator::new(&dict, &mem).evaluate(&q).unwrap() {
            QueryResult::Select(rs) => rs,
            other => panic!("expected SELECT, got {other:?}"),
        };

        let norm = |rs: &crate::query::ResultSet| {
            let mut v: Vec<Vec<Option<String>>> = rs.rows.clone();
            v.sort();
            v
        };
        assert_eq!(lazy.vars, eager.vars);
        assert_eq!(norm(&lazy), norm(&eager));
        assert_eq!(lazy.row_count(), n as usize);
        std::fs::remove_file(&path).ok();
    }

    // ---- VList-compressed value index (task 2) ----------------------------

    /// Build a hub subject with `n` objects under one predicate (object ids end
    /// up consecutive, so the VList compresses to ~1 byte/id).
    fn hub_store(path: &std::path::Path, n: u32) -> DiskStore {
        let mut content = String::new();
        for i in 0..n {
            content.push_str(&format!("<http://ex/hub> <http://ex/p> <http://ex/o{i}> .\n"));
        }
        DiskStore::build_str(path, 64, &content).unwrap()
    }

    #[test]
    fn compact_preserves_o_by_sp_and_compresses() {
        let path = tmp("vlist_compress");
        let n = 500u32;
        let mut ds = hub_store(&path, n);
        let hub = ds.term_id(&Term::iri("http://ex/hub")).unwrap().unwrap();
        let p = ds
            .predicate_id(&Term::iri("http://ex/p").dict_key())
            .unwrap()
            .unwrap();

        // Result before compaction (authoritative SPO scan).
        let before = ds.o_by_sp(hub, p).unwrap();
        assert_eq!(before.len(), n as usize);
        assert!(!ds.is_compacted());
        assert_eq!(ds.compact_value_len(hub, p).unwrap(), None);

        ds.compact().unwrap();
        assert!(ds.is_compacted());

        // Same objects, now served from the VList index.
        let after = ds.o_by_sp(hub, p).unwrap();
        assert_eq!(after, before);

        // The encoded value is far smaller than a raw 4-byte-per-id array.
        let raw = n as usize * 4;
        let enc = ds.compact_value_len(hub, p).unwrap().unwrap();
        assert!(
            enc < raw / 3,
            "VList value should be <1/3 of raw {raw} bytes, got {enc}"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn query_correct_after_compact() {
        let path = tmp("vlist_query");
        let mut ds = hub_store(&path, 30);
        ds.compact().unwrap();
        ds.flush().unwrap();
        // The query path uses o_by_sp, now backed by the VList index.
        let res = ds
            .query("SELECT ?o WHERE { <http://ex/hub> <http://ex/p> ?o }")
            .unwrap();
        match res {
            QueryResult::Select(rs) => assert_eq!(rs.row_count(), 30),
            other => panic!("expected SELECT, got {other:?}"),
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn write_after_compact_invalidates_index() {
        let path = tmp("vlist_invalidate");
        let mut ds = hub_store(&path, 20);
        ds.compact().unwrap();
        assert!(ds.is_compacted());

        // A new triple must clear the flag and still read correctly (via scan).
        let t = Triple::new(
            Term::iri("http://ex/hub"),
            Term::iri("http://ex/p"),
            Term::iri("http://ex/extra"),
        );
        assert!(ds.insert_triple(&t).unwrap());
        assert!(!ds.is_compacted(), "write must invalidate the VList index");

        let hub = ds.term_id(&Term::iri("http://ex/hub")).unwrap().unwrap();
        let p = ds
            .predicate_id(&Term::iri("http://ex/p").dict_key())
            .unwrap()
            .unwrap();
        assert_eq!(ds.o_by_sp(hub, p).unwrap().len(), 21);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn compact_persists_across_reopen() {
        let path = tmp("vlist_reopen");
        let (hub, p);
        {
            let mut ds = hub_store(&path, 40);
            ds.compact().unwrap();
            ds.flush().unwrap();
            hub = ds.term_id(&Term::iri("http://ex/hub")).unwrap().unwrap();
            p = ds
                .predicate_id(&Term::iri("http://ex/p").dict_key())
                .unwrap()
                .unwrap();
        }
        let ds = DiskStore::open(&path, 64).unwrap();
        assert!(ds.is_compacted(), "compacted flag survives reopen");
        assert!(ds.compact_value_len(hub, p).unwrap().is_some());
        assert_eq!(ds.o_by_sp(hub, p).unwrap().len(), 40);
        std::fs::remove_file(&path).ok();
    }

    // ---- VList overflow chains (task 1) -----------------------------------

    #[test]
    fn compact_overflow_large_list_roundtrips() {
        // A single (sub, pred) with thousands of objects: the encoded VList far
        // exceeds an inline B+tree value, so it must spill into an overflow chain
        // and still read back exactly — gStore's VList large-block case.
        let path = tmp("vlist_overflow");
        let n = 6000u32;
        let mut ds = hub_store(&path, n);
        let hub = ds.term_id(&Term::iri("http://ex/hub")).unwrap().unwrap();
        let p = ds
            .predicate_id(&Term::iri("http://ex/p").dict_key())
            .unwrap()
            .unwrap();

        let before = ds.o_by_sp(hub, p).unwrap();
        assert_eq!(before.len(), n as usize);

        ds.compact().unwrap();
        assert!(ds.is_compacted());
        // The group is large enough to need an overflow chain spanning >1 page.
        assert_eq!(ds.compact_value_is_overflow(hub, p).unwrap(), Some(true));
        let logical = ds.compact_value_len(hub, p).unwrap().unwrap();
        assert!(
            logical > super::overflow::CHAIN_PAYLOAD,
            "encoded VList ({logical} B) should span multiple overflow pages"
        );

        // Same objects, now served from the overflow-backed VList index.
        let after = ds.o_by_sp(hub, p).unwrap();
        assert_eq!(after, before);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn overflow_query_correct_after_compact_and_reopen() {
        // End-to-end: a SPARQL query over an overflowed group must return every
        // object, both in-process after compaction and after a reopen.
        let path = tmp("vlist_overflow_query");
        let n = 5000u32;
        let (hub, p);
        {
            let mut ds = hub_store(&path, n);
            ds.compact().unwrap();
            ds.flush().unwrap();
            hub = ds.term_id(&Term::iri("http://ex/hub")).unwrap().unwrap();
            p = ds
                .predicate_id(&Term::iri("http://ex/p").dict_key())
                .unwrap()
                .unwrap();
            assert_eq!(ds.compact_value_is_overflow(hub, p).unwrap(), Some(true));
            let res = ds
                .query("SELECT ?o WHERE { <http://ex/hub> <http://ex/p> ?o }")
                .unwrap();
            match res {
                QueryResult::Select(rs) => assert_eq!(rs.row_count(), n as usize),
                other => panic!("expected SELECT, got {other:?}"),
            }
        }
        // Reopen: the overflow chain + head record persisted; reads still work.
        let ds = DiskStore::open(&path, 64).unwrap();
        assert!(ds.is_compacted());
        assert_eq!(ds.compact_value_is_overflow(hub, p).unwrap(), Some(true));
        assert_eq!(ds.o_by_sp(hub, p).unwrap().len(), n as usize);
        let res = ds
            .query("SELECT ?o WHERE { <http://ex/hub> <http://ex/p> ?o }")
            .unwrap();
        match res {
            QueryResult::Select(rs) => assert_eq!(rs.row_count(), n as usize),
            other => panic!("expected SELECT, got {other:?}"),
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn recompact_frees_overflow_chains_no_unbounded_growth() {
        // Rebuilding the index must free the previous overflow chain rather than
        // leaking its pages, so repeated compaction does not grow the file.
        let path = tmp("vlist_overflow_recompact");
        let mut ds = hub_store(&path, 6000);
        ds.compact().unwrap();
        ds.flush().unwrap();
        let pages_after_first = ds.page_count_for_test();
        for _ in 0..3 {
            ds.compact().unwrap();
        }
        ds.flush().unwrap();
        let pages_after_more = ds.page_count_for_test();
        assert!(
            pages_after_more <= pages_after_first,
            "recompaction leaked overflow pages: {pages_after_first} -> {pages_after_more}"
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn mixed_inline_and_overflow_groups_roundtrip() {
        // One small group (inline) and one huge group (overflow) in the same
        // store must both read back correctly after compaction.
        let path = tmp("vlist_mixed");
        let mut content = String::new();
        for i in 0..5u32 {
            content.push_str(&format!("<http://ex/small> <http://ex/p> <http://ex/o{i}> .\n"));
        }
        for i in 0..6000u32 {
            content.push_str(&format!("<http://ex/big> <http://ex/p> <http://ex/b{i}> .\n"));
        }
        let mut ds = DiskStore::build_str(&path, 64, &content).unwrap();
        let small = ds.term_id(&Term::iri("http://ex/small")).unwrap().unwrap();
        let big = ds.term_id(&Term::iri("http://ex/big")).unwrap().unwrap();
        let p = ds
            .predicate_id(&Term::iri("http://ex/p").dict_key())
            .unwrap()
            .unwrap();
        let small_before = ds.o_by_sp(small, p).unwrap();
        let big_before = ds.o_by_sp(big, p).unwrap();

        ds.compact().unwrap();
        assert_eq!(ds.compact_value_is_overflow(small, p).unwrap(), Some(false));
        assert_eq!(ds.compact_value_is_overflow(big, p).unwrap(), Some(true));
        assert_eq!(ds.o_by_sp(small, p).unwrap(), small_before);
        assert_eq!(ds.o_by_sp(big, p).unwrap(), big_before);
        std::fs::remove_file(&path).ok();
    }

    // ---- integer-keyed id arrays (IVArray, task 2) ------------------------

    #[test]
    fn ivarray_id_stores_match_memory_dictionary() {
        // The id→string stores now use the integer-keyed IvArray. Resolving ids
        // back to strings must be byte-identical to the in-memory dictionary
        // (rebuilt from the same disk store) for entities, literals, predicates.
        let path = tmp("ivarray_parity");
        let ds = DiskStore::build_str(&path, 64, SMALL).unwrap();
        let (dict, _mem) = ds.to_memory().unwrap();
        for id in 0..ds.entity_num() as u32 {
            assert_eq!(
                ds.id_to_string(id).unwrap().as_deref(),
                dict.id_to_string(id),
                "entity id {id}"
            );
        }
        for i in 0..ds.literal_num() as u32 {
            let id = LITERAL_FIRST_ID + i;
            assert_eq!(
                ds.id_to_string(id).unwrap().as_deref(),
                dict.id_to_string(id),
                "literal id {id}"
            );
        }
        for p in 0..ds.predicate_num() as u32 {
            assert_eq!(
                ds.predicate_to_string(p).unwrap().as_deref(),
                dict.predicate_to_string(p),
                "predicate id {p}"
            );
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn ivarray_long_strings_roundtrip_through_disk_query() {
        // A long entity IRI (subject) and a long literal (object) — both far past
        // the IvArray inline capacity — must round-trip through the id→string
        // arrays' overflow chains, end to end via a SPARQL query.
        let path = tmp("ivarray_long");
        let long_iri = format!("http://ex/{}", "z".repeat(400));
        let long_lit = "x".repeat(500);
        let content = format!("<{long_iri}> <http://ex/p> \"{long_lit}\" .\n");
        let ds = DiskStore::build_str(&path, 64, &content).unwrap();

        // Direct id→string round-trip (entity + literal).
        let subj = ds.term_id(&Term::iri(&long_iri)).unwrap().unwrap();
        assert_eq!(ds.id_to_string(subj).unwrap().unwrap(), format!("<{long_iri}>"));

        // Full query path (uses the out-of-core DiskDict, also IvArray-backed).
        let res = ds
            .query("SELECT ?s ?o WHERE { ?s <http://ex/p> ?o }")
            .unwrap();
        match res {
            QueryResult::Select(rs) => {
                assert_eq!(rs.row_count(), 1);
                assert_eq!(rs.rows[0][0].as_deref(), Some(format!("<{long_iri}>").as_str()));
                assert_eq!(rs.rows[0][1].as_deref(), Some(format!("\"{long_lit}\"").as_str()));
            }
            other => panic!("expected SELECT, got {other:?}"),
        }
        std::fs::remove_file(&path).ok();
    }

    // ---- concurrent readers (task 4: RwLock latching) ---------------------

    #[test]
    fn concurrent_readers_get_consistent_results() {
        // `DiskStore` is `Send + Sync`, so an `Arc<DiskStore>` can be shared by
        // many threads that read concurrently through the pager's `RwLock` read
        // guard. Every thread must get correct query answers with no data race,
        // deadlock, or panic.
        use std::sync::Arc;
        use std::thread;

        // Compile-time proof that DiskStore is shareable across threads.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DiskStore>();

        let path = tmp("concurrent_readers");
        let n = 200u32;
        let mut content = String::new();
        for i in 0..n {
            content.push_str(&format!("<http://ex/s{i}> <http://ex/p> <http://ex/o{i}> .\n"));
        }
        let mut ds = DiskStore::build_str(&path, 64, &content).unwrap();
        ds.compact().unwrap();
        ds.flush().unwrap();
        let ds = Arc::new(ds);

        let mut handles = Vec::new();
        for t in 0..8usize {
            let ds = Arc::clone(&ds);
            handles.push(thread::spawn(move || {
                for round in 0..60usize {
                    let i = (t * 17 + round) % n as usize;
                    // Exercises term_id, o_by_sp (VList read), and id_to_string —
                    // all on the shared read path.
                    let res = ds
                        .query(&format!(
                            "SELECT ?o WHERE {{ <http://ex/s{i}> <http://ex/p> ?o }}"
                        ))
                        .unwrap();
                    match res {
                        QueryResult::Select(rs) => {
                            assert_eq!(rs.row_count(), 1);
                            assert_eq!(
                                rs.rows[0][0].as_deref(),
                                Some(format!("<http://ex/o{i}>").as_str())
                            );
                        }
                        other => panic!("expected SELECT, got {other:?}"),
                    }
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        std::fs::remove_file(&path).ok();
    }
}
