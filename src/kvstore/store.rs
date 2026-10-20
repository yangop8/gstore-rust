//! A disk-backed triple store + dictionary built from B+ trees.
//!
//! Mirrors gStore's `KVstore`: the dictionary trees (`entity2id`/`literal2id`/
//! `predicate2id` and their inverses) plus the triple value indexes. Triples are
//! held in three ordered B+ trees — SPO, POS, OSP — with 12-byte composite keys
//! (big-endian `subject|predicate|object` in each ordering). Prefix range scans
//! over these orderings answer every access pattern, exactly as the in-memory
//! [`crate::store::TripleStore`] does, but entirely from disk through the page
//! cache. Data persists and reopens.
//!
//! Named graphs live in a fourth B+ tree, GSPO, keyed by the 16-byte
//! `graph|subject|predicate|object` quad (the graph IRI is an entity id), so a
//! database's named graphs persist in the same `kvstore.kv` file, under the same
//! WAL-protected flush, as its default graph.
//!
//! The store also carries a *VS-tree staleness* flag in its header: every
//! default-graph mutation sets it, in the same pager commit as the mutated pages,
//! so a `vstree.kv` signature index built beside the store is known to be out of
//! date after any update that reached disk (see [`DiskStore::vstree_stale`]).

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock};

use crate::dict::{Dictionary, DiskTermSource};
use crate::error::{GStoreError, Result};
use crate::model::id::{is_literal_id, EntityLiteralId, PredId, LITERAL_FIRST_ID};
use crate::model::{IdTriple, Term, Triple};
use crate::parser::{sparql, turtle};
use crate::query::{Evaluator, QueryResult};
use crate::store::{MutableStore, TripleSource, TripleStore};

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
/// Root of the maintained per-predicate statistics [`IvArray`] (gStore
/// `pre2num`/`pre2sub`/`pre2obj`): predicate id → 12-byte record
/// `[num: u32][distinct_subj: u32][distinct_obj: u32]`, all little-endian. A
/// non-zero root here is also the sentinel that the maintained stats exist (used
/// by [`DiskStore::open`] to lazily rebuild them for a pre-statistics file).
const PRED_STATS: usize = 15;
/// Maintained count of distinct ids that appear as a subject (exact; the
/// in-memory counterpart of `TripleStore::distinct_subjects`).
const ROOT_DISTINCT_SUBJ: usize = 16;
/// Maintained count of distinct ids that appear as an object (exact).
const ROOT_DISTINCT_OBJ: usize = 17;
/// Root of the named-graph quad index: 16-byte `g|s|p|o` keys, empty values.
const GSPO: usize = 18;
/// Maintained number of quads (named-graph triples) in [`GSPO`].
const ROOT_QUAD_COUNT: usize = 19;
/// `1` once a default-graph mutation has made a separately-built VS-tree file
/// stale; cleared by [`DiskStore::mark_vstree_fresh`]. `0` (the value of a file
/// written before this flag existed, which was never mutated after its build)
/// means "fresh".
const ROOT_VSTREE_STALE: usize = 20;

/// Bytes of a per-predicate stats record (`num`, `distinct_subj`, `distinct_obj`).
const PRED_STAT_LEN: usize = 12;

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
    /// Named-graph quads, keyed `graph|sub|pred|obj` (all big-endian ids).
    gspo: BTree,
    quad_count: u64,
    /// Whether a default-graph mutation happened since the VS-tree file beside
    /// this store was last rebuilt (persisted in [`ROOT_VSTREE_STALE`]).
    vstree_stale: bool,
    /// Maintained per-predicate statistics (gStore `pre2num`/`pre2sub`/`pre2obj`),
    /// an integer-keyed dense [`IvArray`] of fixed 12-byte counter records. Kept
    /// current on every insert/delete so the optimizer reads `pred_card` /
    /// `pred_distinct_subj` / `pred_distinct_obj` in `O(1)` instead of scanning a
    /// whole predicate's triples.
    pred_stats: IvArray,
    triple_count: u64,
    entity_count: u32,
    literal_count: u32,
    pred_count: u32,
    /// Exact distinct-subject / distinct-object counts, maintained on
    /// insert/delete via cheap B+tree prefix-existence probes (no per-entity
    /// storage — literal object ids are too large for a dense array). Persisted in
    /// [`ROOT_DISTINCT_SUBJ`] / [`ROOT_DISTINCT_OBJ`].
    distinct_subj: u32,
    distinct_obj: u32,
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
        let distinct_subj = pager.root(ROOT_DISTINCT_SUBJ) as u32;
        let distinct_obj = pager.root(ROOT_DISTINCT_OBJ) as u32;
        let quad_count = pager.root(ROOT_QUAD_COUNT);
        let vstree_stale = pager.root(ROOT_VSTREE_STALE) != 0;
        // A file written before the maintained statistics existed has triples but
        // no `pred_stats` array (its root slot is still zero). Detect that and
        // rebuild the stats once, so an old on-disk database still gets O(1)
        // optimizer statistics. A store built by current code always has a
        // non-zero `pred_stats` root once it holds a triple, so this never fires
        // on the normal build→reopen path.
        let needs_rebuild = triple_count > 0 && pager.root(PRED_STATS) == 0;
        let mut store = DiskStore {
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
            gspo: BTree::new(GSPO),
            quad_count,
            vstree_stale,
            pred_stats: IvArray::new(PRED_STATS),
            triple_count,
            entity_count,
            literal_count,
            pred_count,
            distinct_subj,
            distinct_obj,
            compacted,
            disk_dict: Mutex::new(None),
        };
        if needs_rebuild {
            store.rebuild_stats()?;
        }
        Ok(store)
    }

    /// Build a disk store from RDF files. The format of each file is detected by
    /// its extension exactly as for an in-memory build (N-Triples / Turtle /
    /// N-Quads / TriG / RDF-XML / JSON-LD / RDFa; unknown → Turtle), via
    /// [`crate::parser::parse_file_by_ext`]; quad formats are flattened into the
    /// default graph, matching `Database::build_from_files`.
    pub fn build_files<P1: AsRef<Path>, P2: AsRef<Path>>(
        path: P1,
        cache_pages: usize,
        rdf_files: &[P2],
    ) -> Result<DiskStore> {
        let mut store = DiskStore::open(path, cache_pages)?;
        for f in rdf_files {
            for t in crate::parser::parse_file_by_ext(f)? {
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

    /// Intern an object key in the literal or the entity space.
    fn intern_object(&mut self, key: &str, is_literal: bool) -> Result<EntityLiteralId> {
        if is_literal {
            self.intern_literal(key)
        } else {
            self.intern_entity(key)
        }
    }

    /// Insert one triple; returns `true` if newly added.
    pub fn insert_triple(&mut self, t: &Triple) -> Result<bool> {
        self.insert_keys(
            &t.subject.dict_key(),
            &t.predicate.dict_key(),
            &t.object.dict_key(),
            t.object.is_literal(),
        )
    }

    /// Insert one default-graph triple given its dictionary keys (the
    /// [`Term::dict_key`] surface forms); `o_is_literal` selects the object's id
    /// space. New terms are interned in this store's own dictionary. Returns
    /// `true` if the triple was newly added. This is the write-through entry
    /// point the `Database` facade uses: it works on strings, so the in-memory
    /// working set's ids need not line up with this store's ids.
    pub fn insert_keys(&mut self, s: &str, p: &str, o: &str, o_is_literal: bool) -> Result<bool> {
        let s = self.intern_entity(s)?;
        let p = self.intern_predicate(p)?;
        let o = self.intern_object(o, o_is_literal)?;
        self.insert_ids(IdTriple::new(s, p, o))
    }

    /// Delete one default-graph triple given its dictionary keys (see
    /// [`insert_keys`](Self::insert_keys)). Returns `false` if any term is
    /// unknown or the triple is absent. Dictionary entries are kept.
    pub fn delete_keys(&mut self, s: &str, p: &str, o: &str, o_is_literal: bool) -> Result<bool> {
        match self.lookup_keys(s, p, o, o_is_literal)? {
            Some(t) => self.delete_ids(t),
            None => Ok(false),
        }
    }

    /// Resolve a triple's keys to ids without interning (`None` if any term is
    /// unknown to this store's dictionary).
    fn lookup_keys(
        &self,
        s: &str,
        p: &str,
        o: &str,
        o_is_literal: bool,
    ) -> Result<Option<IdTriple>> {
        let guard = self.pager.read().unwrap();
        let pager = &*guard;
        let Some(s) = self.entity2id.get(pager, s.as_bytes())? else {
            return Ok(None);
        };
        let Some(p) = self.predicate2id.get(pager, p.as_bytes())? else {
            return Ok(None);
        };
        let otree = if o_is_literal {
            &self.literal2id
        } else {
            &self.entity2id
        };
        let Some(o) = otree.get(pager, o.as_bytes())? else {
            return Ok(None);
        };
        Ok(Some(IdTriple::new(de32(&s), de32(&p), de32(&o))))
    }

    // ---- named graphs (GSPO quad index) -----------------------------------

    /// Insert a triple into the named graph `g` (the graph IRI's dictionary key,
    /// e.g. `<http://ex/g>`). Returns `true` if the quad was newly added.
    pub fn insert_quad_keys(
        &mut self,
        g: &str,
        s: &str,
        p: &str,
        o: &str,
        o_is_literal: bool,
    ) -> Result<bool> {
        let g = self.intern_entity(g)?;
        let s = self.intern_entity(s)?;
        let p = self.intern_predicate(p)?;
        let o = self.intern_object(o, o_is_literal)?;
        let key = key4(g, s, p, o);
        let mut guard = self.pager.write().unwrap();
        let pager = &mut *guard;
        if self.gspo.get(pager, &key)?.is_some() {
            return Ok(false);
        }
        self.gspo.insert(pager, &key, b"")?;
        self.quad_count += 1;
        pager.set_root(ROOT_QUAD_COUNT, self.quad_count);
        Ok(true)
    }

    /// Delete a triple from the named graph `g` (see
    /// [`insert_quad_keys`](Self::insert_quad_keys)). Returns `false` if any
    /// term is unknown or the quad is absent.
    pub fn delete_quad_keys(
        &mut self,
        g: &str,
        s: &str,
        p: &str,
        o: &str,
        o_is_literal: bool,
    ) -> Result<bool> {
        let Some(t) = self.lookup_keys(s, p, o, o_is_literal)? else {
            return Ok(false);
        };
        let gid = {
            let guard = self.pager.read().unwrap();
            match self.entity2id.get(&guard, g.as_bytes())? {
                Some(v) => de32(&v),
                None => return Ok(false),
            }
        };
        let key = key4(gid, t.sub, t.pred, t.obj);
        let mut guard = self.pager.write().unwrap();
        let pager = &mut *guard;
        if !self.gspo.delete(pager, &key)? {
            return Ok(false);
        }
        self.quad_count = self.quad_count.saturating_sub(1);
        pager.set_root(ROOT_QUAD_COUNT, self.quad_count);
        Ok(true)
    }

    /// Every named-graph quad as `(graph_id, triple)` in this store's id space,
    /// sorted by graph then `(s, p, o)`.
    pub fn quads(&self) -> Result<Vec<(EntityLiteralId, IdTriple)>> {
        let rows = self.scan(&self.gspo, &[])?;
        Ok(rows
            .iter()
            .map(|k| {
                (
                    de32(&k[0..4]),
                    IdTriple::new(de32(&k[4..8]), de32(&k[8..12]), de32(&k[12..16])),
                )
            })
            .collect())
    }

    /// Number of named-graph quads stored.
    pub fn quad_count(&self) -> u64 {
        self.quad_count
    }

    /// The named graphs as in-memory stores keyed by graph-IRI entity id (ids in
    /// this store's id space, so they line up with [`to_memory`](Self::to_memory)).
    pub fn named_to_memory(&self) -> Result<BTreeMap<u32, TripleStore>> {
        let mut grouped: BTreeMap<u32, Vec<IdTriple>> = BTreeMap::new();
        for (g, t) in self.quads()? {
            grouped.entry(g).or_default().push(t);
        }
        Ok(grouped
            .into_iter()
            .map(|(g, ts)| {
                let mut st = TripleStore::new();
                MutableStore::bulk_load(&mut st, ts);
                (g, st)
            })
            .collect())
    }

    // ---- VS-tree staleness -------------------------------------------------

    /// Whether a default-graph insert/delete happened since the last
    /// [`mark_vstree_fresh`](Self::mark_vstree_fresh): a VS-tree file built over
    /// this store's default graph must then be rebuilt before it is trusted as a
    /// candidate filter. The flag lives in the header, so it reaches disk in the
    /// same WAL-protected commit as the mutation that set it.
    pub fn vstree_stale(&self) -> bool {
        self.vstree_stale
    }

    /// Record that the VS-tree file beside this store was rebuilt from the
    /// current default graph (persisted on the next [`flush`](Self::flush)).
    pub fn mark_vstree_fresh(&mut self) {
        self.vstree_stale = false;
        self.pager.write().unwrap().set_root(ROOT_VSTREE_STALE, 0);
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
        // Detect statistics transitions *before* inserting, while the trees still
        // reflect the pre-insert state: whether `sub` is a brand-new subject, `obj`
        // a brand-new object, and whether this is the first triple for `(sub,pred)`
        // / `(pred,obj)` (which move the per-predicate distinct-subject/object
        // counts). Each is a single O(height) prefix probe, not a scan.
        let sub_is_new = !self.spo.prefix_exists(pager, &be32(t.sub))?;
        let obj_is_new = !self.osp.prefix_exists(pager, &be32(t.obj))?;
        let sp_is_new = !self.spo.prefix_exists(pager, &cat(t.sub, t.pred))?;
        let po_is_new = !self.pos.prefix_exists(pager, &cat(t.pred, t.obj))?;

        // Flag a beside-the-store VS-tree as stale *before* touching any page,
        // so whichever pager commit first carries this mutation carries the flag.
        self.vstree_stale = true;
        pager.set_root(ROOT_VSTREE_STALE, 1);
        self.spo.insert(pager, &spo, b"")?;
        self.pos.insert(pager, &key3(t.pred, t.obj, t.sub), b"")?;
        self.osp.insert(pager, &key3(t.obj, t.sub, t.pred), b"")?;
        self.triple_count += 1;
        pager.set_root(ROOT_TRIPLE_COUNT, self.triple_count);

        // Fold the transitions into the maintained statistics.
        if sub_is_new {
            self.distinct_subj += 1;
            pager.set_root(ROOT_DISTINCT_SUBJ, self.distinct_subj as u64);
        }
        if obj_is_new {
            self.distinct_obj += 1;
            pager.set_root(ROOT_DISTINCT_OBJ, self.distinct_obj as u64);
        }
        let (num, dsub, dobj) = read_pred_stat(&self.pred_stats, pager, t.pred)?;
        write_pred_stat(
            &self.pred_stats,
            pager,
            t.pred,
            num + 1,
            dsub + sp_is_new as u32,
            dobj + po_is_new as u32,
        )?;

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
        self.delete_keys(
            &t.subject.dict_key(),
            &t.predicate.dict_key(),
            &t.object.dict_key(),
            t.object.is_literal(),
        )
    }

    fn delete_ids(&mut self, t: IdTriple) -> Result<bool> {
        let mut guard = self.pager.write().unwrap();
        let pager = &mut *guard;
        let spo = key3(t.sub, t.pred, t.obj);
        if self.spo.get(pager, &spo)?.is_none() {
            return Ok(false);
        }
        self.vstree_stale = true;
        pager.set_root(ROOT_VSTREE_STALE, 1);
        self.spo.delete(pager, &spo)?;
        self.pos.delete(pager, &key3(t.pred, t.obj, t.sub))?;
        self.osp.delete(pager, &key3(t.obj, t.sub, t.pred))?;
        self.triple_count -= 1;
        pager.set_root(ROOT_TRIPLE_COUNT, self.triple_count);

        // Mirror the insert transitions: probe *after* deletion to see whether the
        // last triple carrying each aggregate is gone, and step the maintained
        // statistics down accordingly.
        let sub_gone = !self.spo.prefix_exists(pager, &be32(t.sub))?;
        let obj_gone = !self.osp.prefix_exists(pager, &be32(t.obj))?;
        let sp_gone = !self.spo.prefix_exists(pager, &cat(t.sub, t.pred))?;
        let po_gone = !self.pos.prefix_exists(pager, &cat(t.pred, t.obj))?;
        if sub_gone {
            self.distinct_subj = self.distinct_subj.saturating_sub(1);
            pager.set_root(ROOT_DISTINCT_SUBJ, self.distinct_subj as u64);
        }
        if obj_gone {
            self.distinct_obj = self.distinct_obj.saturating_sub(1);
            pager.set_root(ROOT_DISTINCT_OBJ, self.distinct_obj as u64);
        }
        let (num, dsub, dobj) = read_pred_stat(&self.pred_stats, pager, t.pred)?;
        write_pred_stat(
            &self.pred_stats,
            pager,
            t.pred,
            num.saturating_sub(1),
            dsub.saturating_sub(sp_gone as u32),
            dobj.saturating_sub(po_gone as u32),
        )?;

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
        pager.set_root(ROOT_DISTINCT_SUBJ, self.distinct_subj as u64);
        pager.set_root(ROOT_DISTINCT_OBJ, self.distinct_obj as u64);
        pager.set_root(ROOT_QUAD_COUNT, self.quad_count);
        pager.set_root(ROOT_VSTREE_STALE, self.vstree_stale as u64);
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

    // ---- degree / cardinality statistics (gStore KVstore degree APIs) -----
    //
    // These mirror gStore's degree/statistics surface. The per-predicate and
    // global distinct counts are read from maintained counters in O(1); the
    // per-key degree lookups are degree-bounded prefix counts over the triple
    // B+trees (the analogue of reading a gStore value-list length), never a
    // whole-table scan.

    /// Out-degree of `sub`: number of triples with `sub` as subject (gStore
    /// `getEntityOutDegree`). Degree-bounded prefix count over SPO.
    pub fn subject_degree(&self, sub: EntityLiteralId) -> Result<usize> {
        let pager = self.pager.read().unwrap();
        self.spo.count_prefix(&pager, &be32(sub))
    }

    /// In-degree of `obj`: number of triples with `obj` as object (gStore
    /// `getEntityInDegree`). Degree-bounded prefix count over OSP.
    pub fn object_degree(&self, obj: EntityLiteralId) -> Result<usize> {
        let pager = self.pager.read().unwrap();
        self.osp.count_prefix(&pager, &be32(obj))
    }

    /// Number of objects for `(sub, pred)` — gStore `getSubjectPredicateDegree`.
    /// Degree-bounded prefix count over SPO (independent of VList compaction,
    /// since SPO is always authoritative).
    pub fn subject_predicate_degree(&self, sub: EntityLiteralId, pred: PredId) -> Result<usize> {
        let pager = self.pager.read().unwrap();
        self.spo.count_prefix(&pager, &cat(sub, pred))
    }

    /// Number of subjects for `(obj, pred)` — gStore `getObjectPredicateDegree`.
    /// Degree-bounded prefix count over POS.
    pub fn object_predicate_degree(&self, obj: EntityLiteralId, pred: PredId) -> Result<usize> {
        let pager = self.pager.read().unwrap();
        self.pos.count_prefix(&pager, &cat(pred, obj))
    }

    /// Triples with predicate `pred` (gStore `pre2num`) — maintained, O(1).
    pub fn pred_num(&self, pred: PredId) -> usize {
        let pager = self.pager.read().unwrap();
        read_pred_stat(&self.pred_stats, &pager, pred)
            .map(|(n, _, _)| n as usize)
            .unwrap_or(0)
    }

    /// Distinct subjects with predicate `pred` (gStore `pre2sub`) — maintained.
    pub fn pred_subject_count(&self, pred: PredId) -> usize {
        let pager = self.pager.read().unwrap();
        read_pred_stat(&self.pred_stats, &pager, pred)
            .map(|(_, s, _)| s as usize)
            .unwrap_or(0)
    }

    /// Distinct objects with predicate `pred` (gStore `pre2obj`) — maintained.
    pub fn pred_object_count(&self, pred: PredId) -> usize {
        let pager = self.pager.read().unwrap();
        read_pred_stat(&self.pred_stats, &pager, pred)
            .map(|(_, _, o)| o as usize)
            .unwrap_or(0)
    }

    /// Distinct ids appearing as a subject (exact, maintained).
    pub fn distinct_subject_count(&self) -> usize {
        self.distinct_subj as usize
    }

    /// Distinct ids appearing as an object (exact, maintained).
    pub fn distinct_object_count(&self) -> usize {
        self.distinct_obj as usize
    }

    /// Recompute every maintained statistic (per-predicate `pre2num`/`pre2sub`/
    /// `pre2obj` and the distinct subject/object counts) from the authoritative
    /// SPO index. Called automatically by [`open`](Self::open) for a database
    /// written before the statistics existed; also usable as an explicit repair
    /// after an out-of-band bulk load. Idempotent (recomputing yields the values
    /// the incremental path already maintains).
    pub fn rebuild_stats(&mut self) -> Result<()> {
        // 1. Aggregate from the authoritative triples (one SPO scan).
        let all = self.iter_all()?;
        let np = self.pred_count as usize;
        let mut num = vec![0u32; np];
        let mut psub: Vec<HashSet<u32>> = vec![HashSet::new(); np];
        let mut pobj: Vec<HashSet<u32>> = vec![HashSet::new(); np];
        let mut subs: HashSet<u32> = HashSet::new();
        let mut objs: HashSet<u32> = HashSet::new();
        for t in &all {
            subs.insert(t.sub);
            objs.insert(t.obj);
            if let Some(p) = num.get_mut(t.pred as usize) {
                *p += 1;
                psub[t.pred as usize].insert(t.sub);
                pobj[t.pred as usize].insert(t.obj);
            }
        }

        // 2. Write the recomputed statistics under a single write guard.
        let mut guard = self.pager.write().unwrap();
        let pager = &mut *guard;
        for p in 0..np as u32 {
            write_pred_stat(
                &self.pred_stats,
                pager,
                p,
                num[p as usize],
                psub[p as usize].len() as u32,
                pobj[p as usize].len() as u32,
            )?;
        }
        self.distinct_subj = subs.len() as u32;
        self.distinct_obj = objs.len() as u32;
        pager.set_root(ROOT_DISTINCT_SUBJ, self.distinct_subj as u64);
        pager.set_root(ROOT_DISTINCT_OBJ, self.distinct_obj as u64);
        Ok(())
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
    // Maintained counters keep every statistic O(1) on the hot planning path —
    // and, unlike the previous dictionary-count estimates / per-predicate scans,
    // these are *exact* (they track the triples actually present, incrementally).
    fn distinct_subjects(&self) -> usize {
        self.distinct_subject_count().max(1)
    }
    fn distinct_objects(&self) -> usize {
        self.distinct_object_count()
    }
    fn num_predicates(&self) -> usize {
        self.predicate_num()
    }
    fn pred_card(&self, p: u32) -> usize {
        self.pred_num(p)
    }
    fn pred_distinct_subj(&self, p: u32) -> usize {
        self.pred_subject_count(p)
    }
    fn pred_distinct_obj(&self, p: u32) -> usize {
        self.pred_object_count(p)
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

/// Read a predicate's `(num, distinct_subj, distinct_obj)` stats record from the
/// maintained [`IvArray`], returning `(0, 0, 0)` for a predicate not yet present.
fn read_pred_stat(stats: &IvArray, pager: &Pager, pred: PredId) -> Result<(u32, u32, u32)> {
    match stats.get(pager, pred)? {
        Some(rec) if rec.len() >= PRED_STAT_LEN => Ok((
            u32::from_le_bytes(rec[0..4].try_into().unwrap()),
            u32::from_le_bytes(rec[4..8].try_into().unwrap()),
            u32::from_le_bytes(rec[8..12].try_into().unwrap()),
        )),
        _ => Ok((0, 0, 0)),
    }
}

/// Write a predicate's `(num, distinct_subj, distinct_obj)` stats record. A
/// predicate whose triple count has fallen to zero is removed from the array
/// (its distinct counts are zero too), keeping the array sparse over live
/// predicates.
fn write_pred_stat(
    stats: &IvArray,
    pager: &mut Pager,
    pred: PredId,
    num: u32,
    dsub: u32,
    dobj: u32,
) -> Result<()> {
    if num == 0 {
        stats.delete(pager, pred)?;
        return Ok(());
    }
    let mut rec = [0u8; PRED_STAT_LEN];
    rec[0..4].copy_from_slice(&num.to_le_bytes());
    rec[4..8].copy_from_slice(&dsub.to_le_bytes());
    rec[8..12].copy_from_slice(&dobj.to_le_bytes());
    stats.insert(pager, pred, &rec)
}

/// 12-byte composite key from three ids (big-endian, ordering-preserving).
fn key3(a: u32, b: u32, c: u32) -> [u8; 12] {
    let mut k = [0u8; 12];
    k[0..4].copy_from_slice(&be32(a));
    k[4..8].copy_from_slice(&be32(b));
    k[8..12].copy_from_slice(&be32(c));
    k
}

/// 16-byte quad key `g|s|p|o` (big-endian, ordering-preserving).
fn key4(g: u32, s: u32, p: u32, o: u32) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[0..4].copy_from_slice(&be32(g));
    k[4..16].copy_from_slice(&key3(s, p, o));
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

    // ---- key-level writes, named-graph quads, VS-tree staleness ------------

    #[test]
    fn key_level_writes_persist_with_their_terms() {
        let path = tmp("keys");
        {
            let mut ds = DiskStore::build_str(&path, 64, SMALL).unwrap();
            assert!(ds.insert_keys("<new>", "<rel>", "\"lit\"@en", true).unwrap());
            assert!(!ds.insert_keys("<new>", "<rel>", "\"lit\"@en", true).unwrap(), "dup");
            assert!(ds.delete_keys("<root>", "<contain>", "<node0>", false).unwrap());
            // Unknown terms and wrong id spaces are simply absent.
            assert!(!ds.delete_keys("<ghost>", "<contain>", "<node0>", false).unwrap());
            assert!(!ds.delete_keys("<new>", "<rel>", "\"lit\"@en", false).unwrap());
            ds.flush().unwrap();
        }
        let ds = DiskStore::open(&path, 64).unwrap();
        assert_eq!(ds.triple_count(), 5);
        let (dict, mem) = ds.to_memory().unwrap();
        let s = dict.entity_id("<new>").unwrap();
        let p = dict.predicate_id("<rel>").unwrap();
        let o = dict.literal_id("\"lit\"@en").unwrap();
        assert!(mem.exists(s, p, o));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn named_graph_quads_persist_and_reload() {
        let path = tmp("quads");
        {
            let mut ds = DiskStore::build_str(&path, 64, SMALL).unwrap();
            assert!(ds.insert_quad_keys("<g1>", "<a>", "<p>", "<b>", false).unwrap());
            assert!(ds.insert_quad_keys("<g1>", "<a>", "<p>", "\"x\"", true).unwrap());
            assert!(ds.insert_quad_keys("<g2>", "<root>", "<contain>", "<node0>", false).unwrap());
            assert!(!ds.insert_quad_keys("<g1>", "<a>", "<p>", "<b>", false).unwrap(), "dup");
            assert!(ds.delete_quad_keys("<g1>", "<a>", "<p>", "\"x\"", true).unwrap());
            assert!(!ds.delete_quad_keys("<g9>", "<a>", "<p>", "<b>", false).unwrap());
            ds.flush().unwrap();
        }
        let ds = DiskStore::open(&path, 64).unwrap();
        assert_eq!(ds.quad_count(), 2);
        // Quads never touch the default graph.
        assert_eq!(ds.triple_count(), 5);
        let (dict, _) = ds.to_memory().unwrap();
        let named = ds.named_to_memory().unwrap();
        assert_eq!(named.len(), 2);
        let g1 = dict.entity_id("<g1>").unwrap();
        let (a, p, b) = (
            dict.entity_id("<a>").unwrap(),
            dict.predicate_id("<p>").unwrap(),
            dict.entity_id("<b>").unwrap(),
        );
        assert!(named[&g1].exists(a, p, b));
        assert_eq!(named[&g1].triple_count(), 1);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn vstree_staleness_flag_tracks_default_graph_writes() {
        let path = tmp("stale_flag");
        {
            let mut ds = DiskStore::build_str(&path, 64, SMALL).unwrap();
            assert!(ds.vstree_stale(), "a build's inserts mark it stale");
            ds.mark_vstree_fresh();
            ds.flush().unwrap();
        }
        {
            let mut ds = DiskStore::open(&path, 64).unwrap();
            assert!(!ds.vstree_stale(), "fresh persists");
            // A named-graph write does not affect the default-graph index …
            ds.insert_quad_keys("<g>", "<a>", "<p>", "<b>", false).unwrap();
            assert!(!ds.vstree_stale());
            // … a default-graph delete does, and the flag is committed with it.
            ds.delete_keys("<node1>", "<own>", "<point0>", false).unwrap();
            assert!(ds.vstree_stale());
            ds.flush().unwrap();
        }
        assert!(DiskStore::open(&path, 64).unwrap().vstree_stale());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn build_files_dispatches_on_extension() {
        let dir = std::env::temp_dir().join("gstore_disk_formats_ut");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ttl = dir.join("d.ttl");
        std::fs::write(&ttl, "@prefix ex: <http://ex/> .\nex:a ex:p ex:b , ex:c .\n").unwrap();
        let nq = dir.join("d.nq");
        std::fs::write(&nq, "<http://ex/d> <http://ex/p> <http://ex/e> <http://ex/g> .\n").unwrap();
        let ds = DiskStore::build_files(dir.join("kv"), 64, &[&ttl, &nq]).unwrap();
        assert_eq!(ds.triple_count(), 3);
        let _ = std::fs::remove_dir_all(&dir);
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

    // ---- maintained degree / cardinality statistics -----------------------

    /// Authoritative (scan-based) recomputation of every statistic, to check the
    /// maintained counters against. Independent of the maintained state.
    fn scan_stats(ds: &DiskStore) -> (u32, u32, Vec<(u32, u32, u32)>) {
        let all = ds.iter_all().unwrap();
        let subs: std::collections::HashSet<u32> = all.iter().map(|t| t.sub).collect();
        let objs: std::collections::HashSet<u32> = all.iter().map(|t| t.obj).collect();
        let np = ds.predicate_num();
        let mut per_pred = Vec::with_capacity(np);
        for p in 0..np as u32 {
            let group: Vec<_> = all.iter().filter(|t| t.pred == p).collect();
            let num = group.len() as u32;
            let ds_: std::collections::HashSet<u32> = group.iter().map(|t| t.sub).collect();
            let do_: std::collections::HashSet<u32> = group.iter().map(|t| t.obj).collect();
            per_pred.push((num, ds_.len() as u32, do_.len() as u32));
        }
        (subs.len() as u32, objs.len() as u32, per_pred)
    }

    /// Assert the maintained statistics equal the authoritative scan.
    fn assert_stats_consistent(ds: &DiskStore) {
        let (subs, objs, per_pred) = scan_stats(ds);
        assert_eq!(ds.distinct_subject_count(), subs as usize, "distinct subjects");
        assert_eq!(ds.distinct_object_count(), objs as usize, "distinct objects");
        for (p, &(num, dsub, dobj)) in per_pred.iter().enumerate() {
            let p = p as u32;
            assert_eq!(ds.pred_num(p), num as usize, "pre2num({p})");
            assert_eq!(ds.pred_subject_count(p), dsub as usize, "pre2sub({p})");
            assert_eq!(ds.pred_object_count(p), dobj as usize, "pre2obj({p})");
            // TripleSource facade must agree with the inherent getters.
            assert_eq!(TripleSource::pred_card(ds, p), num as usize);
            assert_eq!(TripleSource::pred_distinct_subj(ds, p), dsub as usize);
            assert_eq!(TripleSource::pred_distinct_obj(ds, p), dobj as usize);
        }
    }

    #[test]
    fn maintained_stats_match_scan_after_build() {
        let path = tmp("stats_build");
        let ds = DiskStore::build_str(&path, 64, SMALL).unwrap();
        assert_stats_consistent(&ds);
        // Spot-check exact values for the known SMALL shape.
        // subjects: root, node1 → 2 distinct; contain has 2 objs (node0,node1).
        let contain = ds.predicate_id(&Term::iri("contain").dict_key()).unwrap().unwrap();
        assert_eq!(ds.pred_num(contain), 2);
        assert_eq!(ds.pred_subject_count(contain), 1); // only <root>
        assert_eq!(ds.pred_object_count(contain), 2); // node0, node1
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn maintained_stats_track_inserts_and_deletes() {
        let path = tmp("stats_mutate");
        let mut ds = DiskStore::build_str(&path, 64, SMALL).unwrap();
        // Insert a fresh triple reusing an existing subject+predicate but a new obj.
        let t = Triple::new(Term::iri("root"), Term::iri("contain"), Term::iri("node9"));
        assert!(ds.insert_triple(&t).unwrap());
        assert_stats_consistent(&ds);
        // Re-inserting the same triple is a no-op and must not double-count.
        assert!(!ds.insert_triple(&t).unwrap());
        assert_stats_consistent(&ds);
        // Delete it again — the (contain,node9) object should drop out of pre2obj.
        assert!(ds.delete_triple(&t).unwrap());
        assert_stats_consistent(&ds);
        // Delete a whole predicate group down to nothing.
        for o in ["point0", "point1"] {
            let t = Triple::new(Term::iri("node1"), Term::iri("own"), Term::iri(o));
            assert!(ds.delete_triple(&t).unwrap());
        }
        assert_stats_consistent(&ds);
        let own = ds.predicate_id(&Term::iri("own").dict_key()).unwrap().unwrap();
        assert_eq!(ds.pred_num(own), 0, "own group fully deleted");
        assert_eq!(ds.pred_subject_count(own), 0);
        assert_eq!(ds.pred_object_count(own), 0);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn maintained_stats_survive_random_churn() {
        // A deterministic pseudo-random insert/delete churn; the maintained stats
        // must equal the authoritative scan at every checkpoint.
        let path = tmp("stats_churn");
        let mut ds = DiskStore::open(&path, 128).unwrap();
        let mut state: std::collections::HashSet<(u32, u32)> = std::collections::HashSet::new();
        let mut seed: u64 = 0x1234_5678;
        let mut rng = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for step in 0..400 {
            // Small id space so subjects/objects/preds recur (drives transitions).
            let s = (rng() % 8) as u32;
            let p = (rng() % 4) as u32;
            let o = (rng() % 8) as u32 + 100; // disjoint object id space
            let t = Triple::new(
                Term::iri(format!("s{s}")),
                Term::iri(format!("p{p}")),
                Term::iri(format!("o{o}")),
            );
            if rng() % 2 == 0 {
                ds.insert_triple(&t).unwrap();
                state.insert((s, p));
            } else {
                ds.delete_triple(&t).unwrap();
            }
            if step % 40 == 0 {
                assert_stats_consistent(&ds);
            }
        }
        assert_stats_consistent(&ds);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn maintained_stats_persist_across_reopen() {
        let path = tmp("stats_reopen");
        {
            let mut ds = DiskStore::build_str(&path, 64, SMALL).unwrap();
            ds.insert_triple(&Triple::new(
                Term::iri("root"),
                Term::iri("own"),
                Term::iri("pointX"),
            ))
            .unwrap();
            ds.flush().unwrap();
        }
        let ds = DiskStore::open(&path, 64).unwrap();
        assert_stats_consistent(&ds);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn rebuild_stats_is_idempotent_and_matches_scan() {
        let path = tmp("stats_rebuild");
        let mut ds = DiskStore::build_str(&path, 64, SMALL).unwrap();
        let before_subj = ds.distinct_subject_count();
        let before_obj = ds.distinct_object_count();
        ds.rebuild_stats().unwrap();
        assert_eq!(ds.distinct_subject_count(), before_subj);
        assert_eq!(ds.distinct_object_count(), before_obj);
        assert_stats_consistent(&ds);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn degree_apis_match_access_patterns() {
        let path = tmp("degree");
        let ds = DiskStore::build_str(&path, 64, SMALL).unwrap();
        let root = ds.term_id(&Term::iri("root")).unwrap().unwrap();
        let node1 = ds.term_id(&Term::iri("node1")).unwrap().unwrap();
        let point0 = ds.term_id(&Term::iri("point0")).unwrap().unwrap();
        let contain = ds.predicate_id(&Term::iri("contain").dict_key()).unwrap().unwrap();
        let own = ds.predicate_id(&Term::iri("own").dict_key()).unwrap().unwrap();

        // subject_degree = number of triples with that subject (out-degree).
        assert_eq!(ds.subject_degree(root).unwrap(), ds.po_by_s(root).unwrap().len());
        assert_eq!(ds.subject_degree(root).unwrap(), 3); // name + contain*2
        assert_eq!(ds.subject_degree(node1).unwrap(), 2); // own*2
        // object_degree = in-degree.
        assert_eq!(ds.object_degree(point0).unwrap(), 1);
        assert_eq!(ds.object_degree(root).unwrap(), 0); // root never an object
        // getSubjectPredicateDegree / getObjectPredicateDegree.
        assert_eq!(
            ds.subject_predicate_degree(root, contain).unwrap(),
            ds.o_by_sp(root, contain).unwrap().len()
        );
        assert_eq!(ds.subject_predicate_degree(root, contain).unwrap(), 2);
        assert_eq!(ds.subject_predicate_degree(node1, own).unwrap(), 2);
        assert_eq!(
            ds.object_predicate_degree(point0, own).unwrap(),
            ds.s_by_po(own, point0).unwrap().len()
        );
        assert_eq!(ds.object_predicate_degree(point0, own).unwrap(), 1);
        // Absent combinations are zero, not errors.
        assert_eq!(ds.subject_predicate_degree(node1, contain).unwrap(), 0);
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
