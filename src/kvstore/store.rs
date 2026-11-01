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
//! Every header slot is assigned in the [`format`](super::format) registry.
//!
//! ## Reads
//! The fallible `try_*` methods mirror every [`TripleSource`] method and return
//! `io::Result`: they decode keys straight out of the cached pages (no per-key
//! allocation) into the output vectors, **in the C1 contract order** of the
//! trait (`po_by_s` → `(p,o)`, `ps_by_o` → `(p,s)`, `so_by_p` → `(s,o)`, all
//! sorted; the id lists ascending; `iter_all` in `(p,s,o)` order) — the same
//! order [`TripleStore`] produces, so plans and solution order do not depend on
//! the backend. The infallible [`TripleSource`] impl delegates to them and
//! turns a read error into an empty answer (a test-only convenience; the
//! database layer reports read errors per query). Exact counts (`count_*`,
//! `distinct_p_of_*`, `has_*`) never materialize a range.
//!
//! ## Writes
//! Every logical write — interning a term, inserting or deleting a triple or a
//! quad, and the key-level wrappers that do both — runs in **one** pager
//! operation bracket while holding the pager write lock for its whole duration
//! (see [`PagerOp`]): no commit can contain half of it, and an error midway
//! rolls the whole operation back (pages, header, and this struct's counters).
//! So a crash can no longer leave the three indexes disagreeing about a triple,
//! or a term in `entity2id` whose id the count root does not cover (which would
//! later be handed out twice).

use std::collections::{BTreeMap, HashSet};
use std::io;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};

use crate::dict::{Dictionary, DiskTermSource};
use crate::error::{GStoreError, Result};
use crate::model::id::{is_literal_id, EntityLiteralId, PredId, LITERAL_FIRST_ID};
use crate::model::{IdTriple, Term, Triple};
use crate::parser::nquads::Quad;
use crate::parser::{import, sparql};
use crate::query::{Evaluator, QueryResult};
use crate::store::{MutableStore, TripleSource, TripleStore};

use super::bptree::{be32, BTree};
use super::disk_dict::DiskDict;
use super::format::{
    ENTITY2ID, GSPO, ID2ENTITY, ID2LITERAL, ID2PREDICATE, LITERAL2ID, OSP, POS, PREDICATE2ID,
    PRED_STATS, ROOT_COMPACTED, ROOT_DISTINCT_OBJ, ROOT_DISTINCT_SUBJ, ROOT_ENTITY_COUNT,
    ROOT_LITERAL_COUNT, ROOT_PRED_COUNT, ROOT_QUAD_COUNT, ROOT_TRIPLE_COUNT, ROOT_VSTREE_STALE,
    SP2O_VLIST, SPO,
};
use super::ivarray::{self, IvArray};
use super::overflow;
use super::pager::{Pager, PagerOp, PAGE_SIZE};
use super::vlist;

/// Bytes of a per-predicate stats record (`num`, `distinct_subj`, `distinct_obj`).
pub(crate) const PRED_STAT_LEN: usize = 12;

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

/// A per-predicate statistics record: `(num, distinct_subj, distinct_obj)`.
type PredStat = (u32, u32, u32);

/// The in-memory counters a write operation may change, restored if it fails.
#[derive(Clone, Copy)]
struct Counters {
    triple_count: u64,
    entity_count: u32,
    literal_count: u32,
    pred_count: u32,
    distinct_subj: u32,
    distinct_obj: u32,
    quad_count: u64,
    vstree_stale: bool,
    compacted: bool,
    nonempty_preds: usize,
    stats_len: usize,
}

/// A disk-backed gStore database (dictionary + six-way triple index).
pub struct DiskStore {
    /// `Arc<RwLock<_>>` (not `RefCell`) so the out-of-core [`DiskDict`] can share
    /// the very same pager/page-cache, and so the type stays `Send`-able. The
    /// `RwLock` lets many readers resolve pages concurrently — every read path
    /// takes `.read()` and the [`Pager`]'s `read_page_shared(&self)` only
    /// briefly latches its internal cache — while every logical write holds
    /// `.write()` for its whole operation.
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
    /// list). Built by [`compact`](DiskStore::compact); read by `o_by_sp`
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
    /// current on every insert/delete.
    pred_stats: IvArray,
    /// In-memory mirror of `pred_stats` (index = predicate id), loaded at open
    /// in `O(P)` and maintained with it, so the optimizer's `pred_card` /
    /// `pred_distinct_subj` / `pred_distinct_obj` read no page at all.
    stats: Vec<PredStat>,
    /// Predicates with at least one triple (exact `num_predicates`).
    nonempty_preds: usize,
    /// `(pred, previous record)` for each mirror entry the current write
    /// operation changed (to undo a failed operation).
    stats_undo: Vec<(PredId, PredStat)>,
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
    /// share it via `Arc` and read concurrently.
    disk_dict: Mutex<Option<Arc<DiskDict>>>,
    /// A view created by [`from_shared_pager`](Self::from_shared_pager): every
    /// write (and commit) is refused.
    read_only: bool,
}

/// A store read error as an `io::Error` (I/O errors keep their kind; anything
/// else, such as a malformed page, is `InvalidData`).
fn to_io(e: GStoreError) -> io::Error {
    match e {
        GStoreError::Io(e) => e,
        other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
    }
}

fn io<T>(r: Result<T>) -> io::Result<T> {
    r.map_err(to_io)
}

fn malformed(what: &str) -> GStoreError {
    GStoreError::Database(format!("corrupt kvstore: {what}"))
}

/// The big-endian `u32` at byte `i` of a key whose length was checked.
#[inline]
fn id_at(k: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([k[i], k[i + 1], k[i + 2], k[i + 3]])
}

/// Decode a 4-byte big-endian id value of a dictionary tree.
fn id_value(v: &[u8]) -> Result<u32> {
    <[u8; 4]>::try_from(v)
        .map(u32::from_be_bytes)
        .map_err(|_| malformed("dictionary value is not a 4-byte id"))
}

/// Look `key` up in a `*2id` tree.
fn lookup_id(tree: &BTree, pager: &Pager, key: &[u8]) -> Result<Option<u32>> {
    tree.get_with(pager, key, id_value)?.transpose()
}

/// Visit the keys of `tree` under `prefix`, each checked to be `len` bytes.
fn scan_keys(
    tree: &BTree,
    pager: &Pager,
    prefix: &[u8],
    len: usize,
    mut f: impl FnMut(&[u8]),
) -> Result<()> {
    let mut bad = false;
    tree.scan_prefix_with(pager, prefix, |k, _| {
        if k.len() != len {
            bad = true;
            return ControlFlow::Break(());
        }
        f(k);
        ControlFlow::Continue(())
    })?;
    if bad {
        return Err(malformed("an index key has the wrong length"));
    }
    Ok(())
}

impl DiskStore {
    /// Open (or create) a disk store at `path` with a `cache_pages`-page cache.
    pub fn open<P: AsRef<Path>>(path: P, cache_pages: usize) -> Result<DiskStore> {
        let pager = Pager::open(path, cache_pages)?;
        // A file written before the maintained statistics existed has triples but
        // no `pred_stats` array (its root slot is still zero). Detect that and
        // rebuild the stats once, so an old on-disk database still gets O(1)
        // optimizer statistics. A store built by current code always has a
        // non-zero `pred_stats` root once it holds a triple, so this never fires
        // on the normal build→reopen path.
        let needs_rebuild = pager.root(ROOT_TRIPLE_COUNT) > 0 && pager.root(PRED_STATS) == 0;
        let mut store = DiskStore::with_pager(Arc::new(RwLock::new(pager)), false)?;
        if needs_rebuild {
            store.rebuild_stats()?;
        }
        Ok(store)
    }

    /// A **read-only** view over a pager another `DiskStore` (the writer) owns:
    /// counts are read from the header roots and the statistics mirror is
    /// loaded from the file as they are now; [`refresh`](Self::refresh)
    /// re-reads them after the writer committed more operations. Every write
    /// and commit through the view is refused.
    pub fn from_shared_pager(pager: Arc<RwLock<Pager>>) -> Result<DiskStore> {
        DiskStore::with_pager(pager, true)
    }

    /// The pager (and page cache) this store reads and writes through.
    pub fn shared_pager(&self) -> Arc<RwLock<Pager>> {
        Arc::clone(&self.pager)
    }

    /// Whether this is a read-only view (see
    /// [`from_shared_pager`](Self::from_shared_pager)).
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn with_pager(pager: Arc<RwLock<Pager>>, read_only: bool) -> Result<DiskStore> {
        let mut store = DiskStore {
            pager,
            // The triple and quad indexes hold fixed-width keys and empty
            // values, so their nodes are classified in O(1) and binary-searched.
            spo: BTree::with_fixed_shape(SPO, 12, 0),
            pos: BTree::with_fixed_shape(POS, 12, 0),
            osp: BTree::with_fixed_shape(OSP, 12, 0),
            entity2id: BTree::new(ENTITY2ID),
            literal2id: BTree::new(LITERAL2ID),
            predicate2id: BTree::new(PREDICATE2ID),
            id2entity: IvArray::new(ID2ENTITY),
            id2literal: IvArray::new(ID2LITERAL),
            id2predicate: IvArray::new(ID2PREDICATE),
            sp2o_vlist: BTree::new(SP2O_VLIST),
            gspo: BTree::with_fixed_shape(GSPO, 16, 0),
            quad_count: 0,
            vstree_stale: false,
            pred_stats: IvArray::new(PRED_STATS),
            stats: Vec::new(),
            nonempty_preds: 0,
            stats_undo: Vec::new(),
            triple_count: 0,
            entity_count: 0,
            literal_count: 0,
            pred_count: 0,
            distinct_subj: 0,
            distinct_obj: 0,
            compacted: false,
            disk_dict: Mutex::new(None),
            read_only,
        };
        store.refresh()?;
        Ok(store)
    }

    /// Re-read every counter from the header roots and reload the statistics
    /// mirror (`O(P)`). A writer's own counters are always current; this is for
    /// a read-only view catching up with its writer.
    pub fn refresh(&mut self) -> Result<()> {
        let pager = self.pager.read().unwrap();
        // The term counts are dense ids into the `id2*` arrays, so a count
        // past their capacity is a damaged header: fail here, before anything
        // is sized from it.
        let dense = |slot: usize, what: &str| -> Result<u32> {
            let n = pager.root(slot);
            if n > ivarray::CAPACITY as u64 {
                return Err(GStoreError::Database(format!(
                    "corrupt kvstore header: {what} count {n} exceeds the id capacity {}",
                    ivarray::CAPACITY
                )));
            }
            Ok(n as u32)
        };
        let (entity_count, literal_count, pred_count) = (
            dense(ROOT_ENTITY_COUNT, "entity")?,
            dense(ROOT_LITERAL_COUNT, "literal")?,
            dense(ROOT_PRED_COUNT, "predicate")?,
        );
        self.triple_count = pager.root(ROOT_TRIPLE_COUNT);
        self.entity_count = entity_count;
        self.literal_count = literal_count;
        self.pred_count = pred_count;
        self.compacted = pager.root(ROOT_COMPACTED) != 0;
        self.distinct_subj = pager.root(ROOT_DISTINCT_SUBJ) as u32;
        self.distinct_obj = pager.root(ROOT_DISTINCT_OBJ) as u32;
        self.quad_count = pager.root(ROOT_QUAD_COUNT);
        self.vstree_stale = pager.root(ROOT_VSTREE_STALE) != 0;
        let mut stats = vec![(0u32, 0u32, 0u32); self.pred_count as usize];
        self.pred_stats
            .for_each_range(&pager, 0, self.pred_count, |p, rec| {
                if let Some(st) = decode_pred_stat(rec) {
                    stats[p as usize] = st;
                }
                ControlFlow::Continue(())
            })?;
        self.nonempty_preds = stats.iter().filter(|s| s.0 > 0).count();
        self.stats = stats;
        self.stats_undo.clear();
        *self.disk_dict.lock().unwrap() = None;
        Ok(())
    }

    /// Build a disk store from RDF files (directories and URLs expand, see
    /// [`import::sources_from_paths`]). The format of each file is detected by
    /// its extension exactly as for an in-memory build (N-Triples / Turtle /
    /// N-Quads / TriG / RDF-XML / JSON-LD / RDFa; unknown → Turtle). The input
    /// streams in; N-Quads / TriG statements keep their named graph (the GSPO
    /// quad index), matching `Database::build_from_files`. Strict.
    pub fn build_files<P1: AsRef<Path>, P2: AsRef<Path>>(
        path: P1,
        cache_pages: usize,
        rdf_files: &[P2],
    ) -> Result<DiskStore> {
        let mut store = DiskStore::open(path, cache_pages)?;
        let sources = import::sources_from_paths(rdf_files)?;
        store.import_sources(&sources, &import::ImportOptions::strict(), &mut ())?;
        store.flush()?;
        Ok(store)
    }

    /// Stream RDF `sources` into this store (see [`crate::parser::import`]):
    /// default-graph statements into the triple indexes, named-graph ones into
    /// the GSPO quad index. Returns the import counts.
    pub fn import_sources(
        &mut self,
        sources: &[import::Source],
        opts: &import::ImportOptions,
        log: &mut dyn import::ErrorLog,
    ) -> Result<import::ImportReport> {
        import::import_sources(
            sources,
            "build",
            opts,
            log,
            &mut |batch: &mut Vec<Quad>| {
                for q in batch.drain(..) {
                    self.insert_quad(&q)?;
                }
                Ok(())
            },
        )
    }

    /// Insert one imported quad (a default-graph triple when it has no graph;
    /// a graph label is an IRI). Returns `true` if newly added.
    fn insert_quad(&mut self, q: &Quad) -> Result<bool> {
        match &q.graph {
            None => self.insert_keys(
                &q.subject.dict_key(),
                &q.predicate.dict_key(),
                &q.object.dict_key(),
                q.object.is_literal(),
            ),
            Some(g) => self.insert_quad_keys(
                &g.dict_key(),
                &q.subject.dict_key(),
                &q.predicate.dict_key(),
                &q.object.dict_key(),
                q.object.is_literal(),
            ),
        }
    }

    /// Build a disk store from an in-memory RDF document (handy for tests).
    pub fn build_str<P: AsRef<Path>>(
        path: P,
        cache_pages: usize,
        content: &str,
    ) -> Result<DiskStore> {
        let mut store = DiskStore::open(path, cache_pages)?;
        let opts = import::ImportOptions {
            blank_nodes: import::BlankNodes::Keep,
            ..import::ImportOptions::strict()
        };
        import::import_reader(
            content.as_bytes(),
            import::RdfFormat::Turtle,
            "<string>",
            &opts,
            &mut (),
            &mut |batch: &mut Vec<Quad>| {
                for q in batch.drain(..) {
                    store.insert_quad(&q)?;
                }
                Ok(())
            },
        )?;
        store.flush()?;
        Ok(store)
    }

    // ---- write operations ---------------------------------------------------

    fn check_writable(&self) -> Result<()> {
        if self.read_only {
            return Err(GStoreError::Database(
                "this kvstore view is read-only".into(),
            ));
        }
        Ok(())
    }

    fn counters(&self) -> Counters {
        Counters {
            triple_count: self.triple_count,
            entity_count: self.entity_count,
            literal_count: self.literal_count,
            pred_count: self.pred_count,
            distinct_subj: self.distinct_subj,
            distinct_obj: self.distinct_obj,
            quad_count: self.quad_count,
            vstree_stale: self.vstree_stale,
            compacted: self.compacted,
            nonempty_preds: self.nonempty_preds,
            stats_len: self.stats.len(),
        }
    }

    fn restore_counters(&mut self, c: Counters) {
        for (p, before) in std::mem::take(&mut self.stats_undo).into_iter().rev() {
            if let Some(slot) = self.stats.get_mut(p as usize) {
                *slot = before;
            }
        }
        self.stats.truncate(c.stats_len);
        self.triple_count = c.triple_count;
        self.entity_count = c.entity_count;
        self.literal_count = c.literal_count;
        self.pred_count = c.pred_count;
        self.distinct_subj = c.distinct_subj;
        self.distinct_obj = c.distinct_obj;
        self.quad_count = c.quad_count;
        self.vstree_stale = c.vstree_stale;
        self.compacted = c.compacted;
        self.nonempty_preds = c.nonempty_preds;
    }

    /// Run one logical write as a single pager operation under the pager write
    /// lock (see the module docs): it commits as a whole or, on error, leaves
    /// no trace in the pages, the header or this struct's counters. A panic
    /// inside the operation (a bug) is rolled back and returned as an error
    /// the same way, instead of unwinding through the write guard: that would
    /// poison the pager for every reader and writer sharing it.
    fn write_op<R>(&mut self, f: impl FnOnce(&mut DiskStore, &mut Pager) -> Result<R>) -> Result<R> {
        self.check_writable()?;
        let before = self.counters();
        self.stats_undo.clear();
        let pager = Arc::clone(&self.pager);
        let mut guard = pager.write().unwrap();
        let mut op = PagerOp::begin(&mut guard);
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self, &mut op)))
            .unwrap_or_else(|panic| {
                let what = panic
                    .downcast_ref::<&str>()
                    .map(|m| m.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                Err(GStoreError::Database(format!(
                    "a disk write panicked and was rolled back: {what}"
                )))
            });
        match outcome {
            Ok(r) => {
                self.stats_undo.clear();
                op.commit()?;
                Ok(r)
            }
            Err(e) => {
                drop(op); // rolls the pages and the header back
                self.restore_counters(before);
                Err(e)
            }
        }
    }

    fn intern_entity_in(&mut self, pager: &mut Pager, key: &str) -> Result<EntityLiteralId> {
        if let Some(id) = lookup_id(&self.entity2id, pager, key.as_bytes())? {
            return Ok(id);
        }
        // Bounded like the in-memory dictionary: an entity id must never spill
        // into the literal range [LITERAL_FIRST_ID, ..).
        let id = crate::dict::checked_entity_id(u64::from(self.entity_count))?;
        self.entity2id.insert(pager, key.as_bytes(), &be32(id))?;
        // id2entity is integer-keyed by the dense entity id.
        self.id2entity.insert(pager, id, key.as_bytes())?;
        self.entity_count += 1;
        pager.set_root(ROOT_ENTITY_COUNT, self.entity_count as u64);
        Ok(id)
    }

    fn intern_literal_in(&mut self, pager: &mut Pager, key: &str) -> Result<EntityLiteralId> {
        if let Some(id) = lookup_id(&self.literal2id, pager, key.as_bytes())? {
            return Ok(id);
        }
        let id = crate::dict::checked_literal_id(u64::from(self.literal_count))?;
        self.literal2id.insert(pager, key.as_bytes(), &be32(id))?;
        // id2literal is integer-keyed by the *local* literal index (id minus the
        // LITERAL_FIRST_ID offset) so the dense array stays compact from 0.
        self.id2literal
            .insert(pager, id - LITERAL_FIRST_ID, key.as_bytes())?;
        self.literal_count += 1;
        pager.set_root(ROOT_LITERAL_COUNT, self.literal_count as u64);
        Ok(id)
    }

    fn intern_predicate_in(&mut self, pager: &mut Pager, key: &str) -> Result<PredId> {
        if let Some(id) = lookup_id(&self.predicate2id, pager, key.as_bytes())? {
            return Ok(id);
        }
        let id = crate::dict::checked_predicate_id(u64::from(self.pred_count))?;
        self.predicate2id
            .insert(pager, key.as_bytes(), &be32(id))?;
        // id2predicate is integer-keyed by the dense predicate id.
        self.id2predicate.insert(pager, id, key.as_bytes())?;
        self.pred_count += 1;
        pager.set_root(ROOT_PRED_COUNT, self.pred_count as u64);
        Ok(id)
    }

    /// Intern an object key in the literal or the entity space.
    fn intern_object_in(
        &mut self,
        pager: &mut Pager,
        key: &str,
        is_literal: bool,
    ) -> Result<EntityLiteralId> {
        if is_literal {
            self.intern_literal_in(pager, key)
        } else {
            self.intern_entity_in(pager, key)
        }
    }

    /// Intern an entity key (one atomic operation); returns its id.
    #[allow(dead_code)] // crate API for the Phase 6 writers (DiskTriples, bulk build)
    pub(crate) fn intern_entity(&mut self, key: &str) -> Result<EntityLiteralId> {
        self.write_op(|st, pg| st.intern_entity_in(pg, key))
    }

    /// Intern a literal key (one atomic operation); returns its public id.
    #[allow(dead_code)] // crate API for the Phase 6 writers (DiskTriples, bulk build)
    pub(crate) fn intern_literal(&mut self, key: &str) -> Result<EntityLiteralId> {
        self.write_op(|st, pg| st.intern_literal_in(pg, key))
    }

    /// Intern a predicate key (one atomic operation); returns its id.
    #[allow(dead_code)] // crate API for the Phase 6 writers (DiskTriples, bulk build)
    pub(crate) fn intern_predicate(&mut self, key: &str) -> Result<PredId> {
        self.write_op(|st, pg| st.intern_predicate_in(pg, key))
    }

    /// Intern an object key in the literal or the entity space.
    #[allow(dead_code)] // crate API for the Phase 6 writers (DiskTriples, bulk build)
    pub(crate) fn intern_object(&mut self, key: &str, is_literal: bool) -> Result<EntityLiteralId> {
        self.write_op(|st, pg| st.intern_object_in(pg, key, is_literal))
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
    /// working set's ids need not line up with this store's ids. The terms and
    /// the triple are one atomic operation.
    pub fn insert_keys(&mut self, s: &str, p: &str, o: &str, o_is_literal: bool) -> Result<bool> {
        self.write_op(|st, pg| {
            let s = st.intern_entity_in(pg, s)?;
            let p = st.intern_predicate_in(pg, p)?;
            let o = st.intern_object_in(pg, o, o_is_literal)?;
            st.insert_ids_in(pg, IdTriple::new(s, p, o))
        })
    }

    /// Delete one default-graph triple given its dictionary keys (see
    /// [`insert_keys`](Self::insert_keys)). Returns `false` if any term is
    /// unknown or the triple is absent. Dictionary entries are kept.
    pub fn delete_keys(&mut self, s: &str, p: &str, o: &str, o_is_literal: bool) -> Result<bool> {
        self.write_op(|st, pg| match st.lookup_keys_in(pg, s, p, o, o_is_literal)? {
            Some(t) => st.delete_ids_in(pg, t),
            None => Ok(false),
        })
    }

    /// Resolve a triple's keys to ids without interning (`None` if any term is
    /// unknown to this store's dictionary).
    fn lookup_keys_in(
        &self,
        pager: &Pager,
        s: &str,
        p: &str,
        o: &str,
        o_is_literal: bool,
    ) -> Result<Option<IdTriple>> {
        let Some(s) = lookup_id(&self.entity2id, pager, s.as_bytes())? else {
            return Ok(None);
        };
        let Some(p) = lookup_id(&self.predicate2id, pager, p.as_bytes())? else {
            return Ok(None);
        };
        let otree = if o_is_literal {
            &self.literal2id
        } else {
            &self.entity2id
        };
        let Some(o) = lookup_id(otree, pager, o.as_bytes())? else {
            return Ok(None);
        };
        Ok(Some(IdTriple::new(s, p, o)))
    }

    // ---- named graphs (GSPO quad index) -----------------------------------

    /// Insert a triple into the named graph `g` (the graph IRI's dictionary key,
    /// e.g. `<http://ex/g>`). Returns `true` if the quad was newly added. The
    /// terms and the quad are one atomic operation.
    pub fn insert_quad_keys(
        &mut self,
        g: &str,
        s: &str,
        p: &str,
        o: &str,
        o_is_literal: bool,
    ) -> Result<bool> {
        self.write_op(|st, pg| {
            let g = st.intern_entity_in(pg, g)?;
            let s = st.intern_entity_in(pg, s)?;
            let p = st.intern_predicate_in(pg, p)?;
            let o = st.intern_object_in(pg, o, o_is_literal)?;
            st.insert_quad_ids_in(pg, g, IdTriple::new(s, p, o))
        })
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
        self.write_op(|st, pg| {
            let Some(t) = st.lookup_keys_in(pg, s, p, o, o_is_literal)? else {
                return Ok(false);
            };
            let Some(gid) = lookup_id(&st.entity2id, pg, g.as_bytes())? else {
                return Ok(false);
            };
            st.delete_quad_ids_in(pg, gid, t)
        })
    }

    fn insert_quad_ids_in(&mut self, pager: &mut Pager, g: EntityLiteralId, t: IdTriple) -> Result<bool> {
        let key = key4(g, t.sub, t.pred, t.obj);
        if self.gspo.contains(pager, &key)? {
            return Ok(false);
        }
        self.gspo.insert(pager, &key, b"")?;
        self.quad_count += 1;
        pager.set_root(ROOT_QUAD_COUNT, self.quad_count);
        Ok(true)
    }

    fn delete_quad_ids_in(&mut self, pager: &mut Pager, g: EntityLiteralId, t: IdTriple) -> Result<bool> {
        if !self.gspo.delete(pager, &key4(g, t.sub, t.pred, t.obj))? {
            return Ok(false);
        }
        self.quad_count = self.quad_count.saturating_sub(1);
        pager.set_root(ROOT_QUAD_COUNT, self.quad_count);
        Ok(true)
    }

    /// Insert the quad `(g, t)` by id (one atomic operation). The ids must be
    /// interned in this store. Returns `true` if newly added.
    #[allow(dead_code)] // crate API for the Phase 6 writers (DiskTriples, bulk build)
    pub(crate) fn insert_quad_ids(&mut self, g: EntityLiteralId, t: IdTriple) -> Result<bool> {
        self.write_op(|st, pg| st.insert_quad_ids_in(pg, g, t))
    }

    /// Delete the quad `(g, t)` by id (one atomic operation). Returns `true` if
    /// it existed.
    #[allow(dead_code)] // crate API for the Phase 6 writers (DiskTriples, bulk build)
    pub(crate) fn delete_quad_ids(&mut self, g: EntityLiteralId, t: IdTriple) -> Result<bool> {
        self.write_op(|st, pg| st.delete_quad_ids_in(pg, g, t))
    }

    /// Every named-graph quad as `(graph_id, triple)` in this store's id space,
    /// sorted by graph then `(s, p, o)`.
    pub fn quads(&self) -> Result<Vec<(EntityLiteralId, IdTriple)>> {
        let pager = self.pager.read().unwrap();
        let mut out = with_hint(self.quad_count);
        scan_keys(&self.gspo, &pager, &[], 16, |k| {
            out.push((
                id_at(k, 0),
                IdTriple::new(id_at(k, 4), id_at(k, 8), id_at(k, 12)),
            ));
        })?;
        Ok(out)
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
        if self.read_only {
            return;
        }
        self.vstree_stale = false;
        self.pager
            .write()
            .unwrap()
            .set_root(ROOT_VSTREE_STALE, 0);
    }

    /// Insert one triple by id: the three indexes, the counters and the
    /// statistics, inside the caller's operation.
    fn insert_ids_in(&mut self, pager: &mut Pager, t: IdTriple) -> Result<bool> {
        let spo = key3(t.sub, t.pred, t.obj);
        if self.spo.contains(pager, &spo)? {
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

        // Flag a beside-the-store VS-tree as stale with this mutation (both land
        // in the same commit: the operation is never split).
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
        let (num, dsub, dobj) = self.stat(t.pred);
        self.set_pred_stat(
            pager,
            t.pred,
            (num + 1, dsub + sp_is_new as u32, dobj + po_is_new as u32),
        )?;

        if self.compacted {
            // The VList index no longer reflects the data; reads must rescan.
            self.compacted = false;
            pager.set_root(ROOT_COMPACTED, 0);
        }
        Ok(true)
    }

    /// Delete one triple by id from all three indexes, inside the caller's
    /// operation.
    fn delete_ids_in(&mut self, pager: &mut Pager, t: IdTriple) -> Result<bool> {
        let spo = key3(t.sub, t.pred, t.obj);
        if !self.spo.contains(pager, &spo)? {
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
        let (num, dsub, dobj) = self.stat(t.pred);
        self.set_pred_stat(
            pager,
            t.pred,
            (
                num.saturating_sub(1),
                dsub.saturating_sub(sp_gone as u32),
                dobj.saturating_sub(po_gone as u32),
            ),
        )?;

        if self.compacted {
            self.compacted = false;
            pager.set_root(ROOT_COMPACTED, 0);
        }
        Ok(true)
    }

    /// Insert one default-graph triple by id (one atomic operation). The ids
    /// must be interned in this store. Returns `true` if newly added.
    #[allow(dead_code)] // crate API for the Phase 6 writers (DiskTriples, bulk build)
    pub(crate) fn insert_ids(&mut self, t: IdTriple) -> Result<bool> {
        self.write_op(|st, pg| st.insert_ids_in(pg, t))
    }

    /// Delete one triple from all three indexes (gStore `KVstore::removeTriple`),
    /// by id, as one atomic operation. Returns `true` if it existed. Dictionary
    /// entries are intentionally kept (gStore does not reclaim string ids on
    /// triple deletion); only the triple indexes shrink, with the B+ trees
    /// merging underfull nodes and freeing pages.
    #[allow(dead_code)] // crate API for the Phase 6 writers (DiskTriples, bulk build)
    pub(crate) fn delete_ids(&mut self, t: IdTriple) -> Result<bool> {
        self.write_op(|st, pg| st.delete_ids_in(pg, t))
    }

    /// Delete one triple given its terms (see [`delete_ids`](Self::delete_ids)).
    /// Returns `false` (no change) if any term is unknown or the triple is
    /// absent.
    pub fn delete_triple(&mut self, t: &Triple) -> Result<bool> {
        self.delete_keys(
            &t.subject.dict_key(),
            &t.predicate.dict_key(),
            &t.object.dict_key(),
            t.object.is_literal(),
        )
    }

    /// The mirrored statistics record of `pred` (`(0,0,0)` if it has none).
    fn stat(&self, pred: PredId) -> PredStat {
        self.stats.get(pred as usize).copied().unwrap_or((0, 0, 0))
    }

    /// Write `pred`'s statistics record to the file and the mirror.
    fn set_pred_stat(&mut self, pager: &mut Pager, pred: PredId, st: PredStat) -> Result<()> {
        write_pred_stat(&self.pred_stats, pager, pred, st)?;
        let i = pred as usize;
        if self.stats.len() <= i {
            self.stats.resize(i + 1, (0, 0, 0));
        }
        let before = std::mem::replace(&mut self.stats[i], st);
        self.stats_undo.push((pred, before));
        match (before.0 > 0, st.0 > 0) {
            (false, true) => self.nonempty_preds += 1,
            (true, false) => self.nonempty_preds -= 1,
            _ => {}
        }
        Ok(())
    }

    /// Commit every completed operation durably (one WAL-protected pager
    /// flush). Through `&self`: every operation keeps the header roots
    /// current, so there is nothing else to write. Refused on a read-only view.
    pub fn commit(&self) -> Result<()> {
        self.check_writable()?;
        self.pager.write().unwrap().flush()
    }

    /// Persist counters and flush all dirty pages (see [`commit`](Self::commit)).
    pub fn flush(&mut self) -> Result<()> {
        self.check_writable()?;
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
        self.stat(pred).0 as usize
    }

    /// Distinct subjects with predicate `pred` (gStore `pre2sub`) — maintained.
    pub fn pred_subject_count(&self, pred: PredId) -> usize {
        self.stat(pred).1 as usize
    }

    /// Distinct objects with predicate `pred` (gStore `pre2obj`) — maintained.
    pub fn pred_object_count(&self, pred: PredId) -> usize {
        self.stat(pred).2 as usize
    }

    /// Distinct ids appearing as a subject (exact, maintained).
    pub fn distinct_subject_count(&self) -> usize {
        self.distinct_subj as usize
    }

    /// Distinct ids appearing as an object (exact, maintained).
    pub fn distinct_object_count(&self) -> usize {
        self.distinct_obj as usize
    }

    /// Predicates with at least one triple (exact, maintained).
    pub fn nonempty_predicate_count(&self) -> usize {
        self.nonempty_preds
    }

    /// Recompute every maintained statistic (per-predicate `pre2num`/`pre2sub`/
    /// `pre2obj` and the distinct subject/object counts) from the authoritative
    /// SPO index. Called automatically by [`open`](Self::open) for a database
    /// written before the statistics existed; also usable as an explicit repair
    /// after an out-of-band bulk load. Idempotent (recomputing yields the values
    /// the incremental path already maintains).
    pub fn rebuild_stats(&mut self) -> Result<()> {
        // 1. Aggregate from the authoritative triples (one SPO scan).
        let all = self.try_iter_spo().map_err(GStoreError::Io)?;
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

        // 2. Write the recomputed statistics as one operation.
        self.write_op(|st, pager| {
            for p in 0..np as u32 {
                let i = p as usize;
                st.set_pred_stat(
                    pager,
                    p,
                    (num[i], psub[i].len() as u32, pobj[i].len() as u32),
                )?;
            }
            st.distinct_subj = subs.len() as u32;
            st.distinct_obj = objs.len() as u32;
            pager.set_root(ROOT_DISTINCT_SUBJ, st.distinct_subj as u64);
            pager.set_root(ROOT_DISTINCT_OBJ, st.distinct_obj as u64);
            Ok(())
        })
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
        lookup_id(tree, &pager, key.as_bytes())
    }

    pub fn predicate_id(&self, dict_key: &str) -> Result<Option<PredId>> {
        let pager = self.pager.read().unwrap();
        lookup_id(&self.predicate2id, &pager, dict_key.as_bytes())
    }

    pub fn id_to_string(&self, id: EntityLiteralId) -> Result<Option<String>> {
        let pager = self.pager.read().unwrap();
        // Literals are keyed by their local index (see `intern_literal`).
        let (arr, key) = if is_literal_id(id) {
            (&self.id2literal, id - LITERAL_FIRST_ID)
        } else {
            (&self.id2entity, id)
        };
        arr.get_with(&pager, key, |b| String::from_utf8_lossy(b).into_owned())
    }

    pub fn predicate_to_string(&self, id: PredId) -> Result<Option<String>> {
        let pager = self.pager.read().unwrap();
        self.id2predicate
            .get_with(&pager, id, |b| String::from_utf8_lossy(b).into_owned())
    }

    // ---- fallible reads in the C1 contract order -------------------------

    fn read_pager(&self) -> RwLockReadGuard<'_, Pager> {
        self.pager.read().unwrap()
    }

    /// `s p o` — does this exact triple exist?
    pub fn try_exists(&self, s: EntityLiteralId, p: PredId, o: EntityLiteralId) -> io::Result<bool> {
        io(self.spo.contains(&self.read_pager(), &key3(s, p, o)))
    }

    /// `s ? ?` → `(pred, obj)` pairs, sorted.
    pub fn try_po_by_s(&self, s: EntityLiteralId) -> io::Result<Vec<(PredId, EntityLiteralId)>> {
        let mut out = Vec::new();
        io(scan_keys(&self.spo, &self.read_pager(), &be32(s), 12, |k| {
            out.push((id_at(k, 4), id_at(k, 8)));
        }))?;
        Ok(out)
    }

    /// `s p ?` → objects, ascending. When the store has been
    /// [`compact`](Self::compact)ed, this is a point lookup into the VList index
    /// and a varint decode (following an [`overflow`] page chain for an oversize
    /// group), instead of a multi-key prefix scan; it transparently falls back
    /// to the SPO scan for any group not present in the index.
    pub fn try_o_by_sp(&self, s: EntityLiteralId, p: PredId) -> io::Result<Vec<EntityLiteralId>> {
        let pager = self.read_pager();
        // The flag in the shared header, not this struct's copy: a read-only
        // view must see the writer's invalidation at once, as its other reads
        // see the writer's pages.
        if pager.root(ROOT_COMPACTED) != 0 {
            if let Some(rec) = io(self.sp2o_vlist.get(&pager, &cat(s, p)))? {
                let bytes = io(materialize_vlist(&pager, &rec))?;
                return Ok(vlist::decode_u32s(&bytes).unwrap_or_default());
            }
        }
        let mut out = Vec::new();
        io(scan_keys(&self.spo, &pager, &cat(s, p), 12, |k| {
            out.push(id_at(k, 8));
        }))?;
        Ok(out)
    }

    /// `s ? o` → predicates linking subject to object, ascending.
    pub fn try_p_by_so(&self, s: EntityLiteralId, o: EntityLiteralId) -> io::Result<Vec<PredId>> {
        // OSP key = (o, s, p); prefix (o, s) → p.
        let mut out = Vec::new();
        io(scan_keys(&self.osp, &self.read_pager(), &cat(o, s), 12, |k| {
            out.push(id_at(k, 8));
        }))?;
        Ok(out)
    }

    /// `? ? o` → `(pred, sub)` pairs, sorted.
    pub fn try_ps_by_o(&self, o: EntityLiteralId) -> io::Result<Vec<(PredId, EntityLiteralId)>> {
        // OSP key = (o, s, p): the scan yields (s, p) order; sort to (p, s).
        let mut out = Vec::new();
        io(scan_keys(&self.osp, &self.read_pager(), &be32(o), 12, |k| {
            out.push((id_at(k, 8), id_at(k, 4)));
        }))?;
        out.sort_unstable();
        Ok(out)
    }

    /// `? p o` → subjects, ascending.
    pub fn try_s_by_po(&self, p: PredId, o: EntityLiteralId) -> io::Result<Vec<EntityLiteralId>> {
        let mut out = Vec::new();
        io(scan_keys(&self.pos, &self.read_pager(), &cat(p, o), 12, |k| {
            out.push(id_at(k, 8));
        }))?;
        Ok(out)
    }

    /// `? p ?` → `(sub, obj)` pairs, sorted.
    pub fn try_so_by_p(&self, p: PredId) -> io::Result<Vec<(EntityLiteralId, EntityLiteralId)>> {
        // POS key = (p, o, s): the scan yields (o, s) order; sort to (s, o).
        let mut out = with_hint(self.stat(p).0.into());
        io(scan_keys(&self.pos, &self.read_pager(), &be32(p), 12, |k| {
            out.push((id_at(k, 8), id_at(k, 4)));
        }))?;
        out.sort_unstable();
        Ok(out)
    }

    /// Distinct subjects of predicate `p`, ascending.
    pub fn try_subs_by_p(&self, p: PredId) -> io::Result<Vec<EntityLiteralId>> {
        let mut out = with_hint(self.stat(p).1.into());
        io(scan_keys(&self.pos, &self.read_pager(), &be32(p), 12, |k| {
            out.push(id_at(k, 8));
        }))?;
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// Distinct objects of predicate `p`, ascending (POS order needs no sort).
    pub fn try_objs_by_p(&self, p: PredId) -> io::Result<Vec<EntityLiteralId>> {
        let mut out: Vec<u32> = with_hint(self.stat(p).2.into());
        io(scan_keys(&self.pos, &self.read_pager(), &be32(p), 12, |k| {
            let o = id_at(k, 4);
            if out.last() != Some(&o) {
                out.push(o);
            }
        }))?;
        Ok(out)
    }

    /// Every id used as a subject, ascending: one streaming pass over SPO.
    pub fn try_subject_keys(&self) -> io::Result<Vec<EntityLiteralId>> {
        let mut out: Vec<u32> = with_hint(self.distinct_subj.into());
        io(scan_keys(&self.spo, &self.read_pager(), &[], 12, |k| {
            let s = id_at(k, 0);
            if out.last() != Some(&s) {
                out.push(s);
            }
        }))?;
        Ok(out)
    }

    /// Every id used as an object, ascending: one streaming pass over OSP.
    pub fn try_object_keys(&self) -> io::Result<Vec<EntityLiteralId>> {
        let mut out: Vec<u32> = with_hint(self.distinct_obj.into());
        io(scan_keys(&self.osp, &self.read_pager(), &[], 12, |k| {
            let o = id_at(k, 0);
            if out.last() != Some(&o) {
                out.push(o);
            }
        }))?;
        Ok(out)
    }

    /// Every triple in SPO order (the cheapest full scan).
    pub fn try_iter_spo(&self) -> io::Result<Vec<IdTriple>> {
        let mut out = with_hint(self.triple_count);
        io(scan_keys(&self.spo, &self.read_pager(), &[], 12, |k| {
            out.push(IdTriple::new(id_at(k, 0), id_at(k, 4), id_at(k, 8)));
        }))?;
        Ok(out)
    }

    /// Every triple in the contract order `(p, s, o)`: the SPO scan, then a
    /// stable bucketing by predicate (within a predicate, SPO order already is
    /// `(s, o)` order), so no comparison sort runs.
    pub fn try_iter_all(&self) -> io::Result<Vec<IdTriple>> {
        let spo = self.try_iter_spo()?;
        let np = spo.iter().map(|t| t.pred as usize + 1).max().unwrap_or(0);
        let mut start = vec![0usize; np + 1];
        for t in &spo {
            start[t.pred as usize + 1] += 1;
        }
        for i in 0..np {
            start[i + 1] += start[i];
        }
        let mut out = vec![IdTriple::new(0, 0, 0); spo.len()];
        for t in spo {
            let slot = &mut start[t.pred as usize];
            out[*slot] = t;
            *slot += 1;
        }
        Ok(out)
    }

    pub fn try_triple_count(&self) -> io::Result<u64> {
        Ok(self.triple_count)
    }
    /// Exact number of distinct subjects.
    pub fn try_distinct_subjects(&self) -> io::Result<usize> {
        Ok(self.distinct_subj as usize)
    }
    /// Exact number of distinct objects.
    pub fn try_distinct_objects(&self) -> io::Result<usize> {
        Ok(self.distinct_obj as usize)
    }
    /// Number of predicates with at least one triple.
    pub fn try_num_predicates(&self) -> io::Result<usize> {
        Ok(self.nonempty_preds)
    }
    pub fn try_pred_card(&self, p: PredId) -> io::Result<usize> {
        Ok(self.pred_num(p))
    }
    pub fn try_pred_distinct_subj(&self, p: PredId) -> io::Result<usize> {
        Ok(self.pred_subject_count(p))
    }
    pub fn try_pred_distinct_obj(&self, p: PredId) -> io::Result<usize> {
        Ok(self.pred_object_count(p))
    }

    // ---- exact counts without materializing (C1) -------------------------

    fn count(&self, tree: &BTree, prefix: &[u8]) -> io::Result<u64> {
        io(tree.count_prefix(&self.read_pager(), prefix)).map(|n| n as u64)
    }

    /// `len(po_by_s(s))`.
    pub fn try_count_po_by_s(&self, s: EntityLiteralId) -> io::Result<u64> {
        self.count(&self.spo, &be32(s))
    }
    /// `len(o_by_sp(s, p))`.
    pub fn try_count_o_by_sp(&self, s: EntityLiteralId, p: PredId) -> io::Result<u64> {
        self.count(&self.spo, &cat(s, p))
    }
    /// `len(p_by_so(s, o))`.
    pub fn try_count_p_by_so(&self, s: EntityLiteralId, o: EntityLiteralId) -> io::Result<u64> {
        self.count(&self.osp, &cat(o, s))
    }
    /// `len(ps_by_o(o))`.
    pub fn try_count_ps_by_o(&self, o: EntityLiteralId) -> io::Result<u64> {
        self.count(&self.osp, &be32(o))
    }
    /// `len(s_by_po(p, o))`.
    pub fn try_count_s_by_po(&self, p: PredId, o: EntityLiteralId) -> io::Result<u64> {
        self.count(&self.pos, &cat(p, o))
    }
    /// `len(so_by_p(p))`, from the maintained statistics.
    pub fn try_count_so_by_p(&self, p: PredId) -> io::Result<u64> {
        Ok(u64::from(self.stat(p).0))
    }

    /// `len(po_by_s(s))`; a read error counts as an empty range (like the
    /// legacy [`TripleSource`] impl).
    pub fn count_po_by_s(&self, s: EntityLiteralId) -> u64 {
        self.try_count_po_by_s(s).unwrap_or(0)
    }
    /// `len(o_by_sp(s, p))` (0 on a read error).
    pub fn count_o_by_sp(&self, s: EntityLiteralId, p: PredId) -> u64 {
        self.try_count_o_by_sp(s, p).unwrap_or(0)
    }
    /// `len(p_by_so(s, o))` (0 on a read error).
    pub fn count_p_by_so(&self, s: EntityLiteralId, o: EntityLiteralId) -> u64 {
        self.try_count_p_by_so(s, o).unwrap_or(0)
    }
    /// `len(ps_by_o(o))` (0 on a read error).
    pub fn count_ps_by_o(&self, o: EntityLiteralId) -> u64 {
        self.try_count_ps_by_o(o).unwrap_or(0)
    }
    /// `len(s_by_po(p, o))` (0 on a read error).
    pub fn count_s_by_po(&self, p: PredId, o: EntityLiteralId) -> u64 {
        self.try_count_s_by_po(p, o).unwrap_or(0)
    }
    /// `len(so_by_p(p))`.
    pub fn count_so_by_p(&self, p: PredId) -> u64 {
        u64::from(self.stat(p).0)
    }

    /// Number of distinct predicates among `po_by_s(s)`.
    pub fn try_distinct_p_of_s(&self, s: EntityLiteralId) -> io::Result<u64> {
        let (mut n, mut last) = (0u64, None);
        io(scan_keys(&self.spo, &self.read_pager(), &be32(s), 12, |k| {
            let p = id_at(k, 4);
            if last != Some(p) {
                n += 1;
                last = Some(p);
            }
        }))?;
        Ok(n)
    }

    /// Number of distinct predicates among `ps_by_o(o)`.
    pub fn try_distinct_p_of_o(&self, o: EntityLiteralId) -> io::Result<u64> {
        let mut ps: Vec<u32> = Vec::new();
        io(scan_keys(&self.osp, &self.read_pager(), &be32(o), 12, |k| {
            ps.push(id_at(k, 8));
        }))?;
        ps.sort_unstable();
        ps.dedup();
        Ok(ps.len() as u64)
    }

    /// Number of distinct predicates of subject `s` (0 on a read error).
    pub fn distinct_p_of_s(&self, s: EntityLiteralId) -> u64 {
        self.try_distinct_p_of_s(s).unwrap_or(0)
    }

    /// Number of distinct predicates of object `o` (0 on a read error).
    pub fn distinct_p_of_o(&self, o: EntityLiteralId) -> u64 {
        self.try_distinct_p_of_o(o).unwrap_or(0)
    }

    /// Whether `id` is the subject of some triple. `true` on a read error:
    /// never free an id when in doubt.
    pub fn has_subject(&self, id: EntityLiteralId) -> bool {
        self.spo
            .prefix_exists(&self.read_pager(), &be32(id))
            .unwrap_or(true)
    }

    /// Whether `id` is the object of some triple (`true` on a read error).
    pub fn has_object(&self, id: EntityLiteralId) -> bool {
        self.osp
            .prefix_exists(&self.read_pager(), &be32(id))
            .unwrap_or(true)
    }

    /// Whether predicate `p` has some triple (`true` on a read error).
    pub fn has_predicate(&self, p: PredId) -> bool {
        self.pos
            .prefix_exists(&self.read_pager(), &be32(p))
            .unwrap_or(true)
    }

    /// One merged, streaming pass over SPO and OSP for VS-tree (re)builds:
    /// `f(entity, out_edges, in_edges)` for every entity that is a subject or
    /// an entity object, in ascending id order, where `out_edges` are its
    /// `(p, o)` pairs (= `po_by_s`) and `in_edges` its `(p, s)` pairs
    /// (= `ps_by_o`), both sorted. Literal objects are not visited (the VS-tree
    /// indexes entities only). `f` may stop the pass with
    /// [`ControlFlow::Break`]. Holds the pager read lock for the whole pass.
    pub fn for_each_entity_adjacency(
        &self,
        mut f: impl FnMut(
            EntityLiteralId,
            &[(PredId, EntityLiteralId)],
            &[(PredId, EntityLiteralId)],
        ) -> ControlFlow<()>,
    ) -> io::Result<()> {
        let pager = self.read_pager();
        let mut spo = io(self.spo.cursor_ge(&pager, &[]))?;
        let mut osp = io(self.osp.cursor_ge(&pager, &[]))?;
        let next = |cur: &mut super::bptree::LeafCursor| -> io::Result<Option<[u32; 3]>> {
            match io(cur.next_entry(&pager))? {
                None => Ok(None),
                Some((k, _)) if k.len() == 12 => Ok(Some([id_at(k, 0), id_at(k, 4), id_at(k, 8)])),
                Some(_) => Err(to_io(malformed("an index key has the wrong length"))),
            }
        };
        // OSP keys of literal objects sort after every entity object.
        let next_entity_obj = |cur: &mut super::bptree::LeafCursor| -> io::Result<Option<[u32; 3]>> {
            Ok(next(cur)?.filter(|k| !is_literal_id(k[0])))
        };
        let mut s_head = next(&mut spo)?;
        let mut o_head = next_entity_obj(&mut osp)?;
        let (mut out, mut inn) = (Vec::new(), Vec::new());
        loop {
            let id = match (s_head, o_head) {
                (None, None) => return Ok(()),
                (Some(a), None) => a[0],
                (None, Some(b)) => b[0],
                (Some(a), Some(b)) => a[0].min(b[0]),
            };
            out.clear();
            inn.clear();
            while let Some(k) = s_head.filter(|k| k[0] == id) {
                out.push((k[1], k[2]));
                s_head = next(&mut spo)?;
            }
            while let Some(k) = o_head.filter(|k| k[0] == id) {
                inn.push((k[2], k[1]));
                o_head = next_entity_obj(&mut osp)?;
            }
            inn.sort_unstable();
            if f(id, &out, &inn).is_break() {
                return Ok(());
            }
        }
    }

    // ---- legacy access patterns (crate `Result`, contract order) ---------

    pub fn exists(&self, s: EntityLiteralId, p: PredId, o: EntityLiteralId) -> Result<bool> {
        self.try_exists(s, p, o).map_err(GStoreError::Io)
    }

    /// `s ? ?` → `(pred, obj)` pairs (sorted by (pred, obj)).
    pub fn po_by_s(&self, s: EntityLiteralId) -> Result<Vec<(PredId, EntityLiteralId)>> {
        self.try_po_by_s(s).map_err(GStoreError::Io)
    }

    /// `s p ?` → objects (see [`try_o_by_sp`](Self::try_o_by_sp)).
    pub fn o_by_sp(&self, s: EntityLiteralId, p: PredId) -> Result<Vec<EntityLiteralId>> {
        self.try_o_by_sp(s, p).map_err(GStoreError::Io)
    }

    /// `? p o` → subjects.
    pub fn s_by_po(&self, p: PredId, o: EntityLiteralId) -> Result<Vec<EntityLiteralId>> {
        self.try_s_by_po(p, o).map_err(GStoreError::Io)
    }

    /// `? p ?` → `(sub, obj)` pairs, sorted.
    pub fn so_by_p(&self, p: PredId) -> Result<Vec<(EntityLiteralId, EntityLiteralId)>> {
        self.try_so_by_p(p).map_err(GStoreError::Io)
    }

    /// `? ? o` → `(pred, sub)` pairs, sorted.
    pub fn ps_by_o(&self, o: EntityLiteralId) -> Result<Vec<(PredId, EntityLiteralId)>> {
        self.try_ps_by_o(o).map_err(GStoreError::Io)
    }

    /// `s ? o` → predicates linking subject to object.
    pub fn p_by_so(&self, s: EntityLiteralId, o: EntityLiteralId) -> Result<Vec<PredId>> {
        self.try_p_by_so(s, o).map_err(GStoreError::Io)
    }

    /// Every triple, in `(p, s, o)` order.
    pub fn iter_all(&self) -> Result<Vec<IdTriple>> {
        self.try_iter_all().map_err(GStoreError::Io)
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
    /// read-side optimization that never returns stale data. Each step is its
    /// own operation: a crash midway leaves the flag clear (reads use SPO).
    pub fn compact(&mut self) -> Result<()> {
        // 0. Mark the index stale up-front: if a crash interrupts the rebuild
        //    below, the next open falls back to the authoritative SPO scan.
        self.write_op(|st, pager| {
            st.compacted = false;
            pager.set_root(ROOT_COMPACTED, 0);
            Ok(())
        })?;

        // 1. Drop any previous VList entries so vanished groups can't be read,
        //    freeing each overflow chain so its pages return to the free list.
        let stale: Vec<(Vec<u8>, Vec<u8>)> = {
            let guard = self.pager.read().unwrap();
            self.sp2o_vlist.iter_all(&guard)?
        };
        for (k, v) in &stale {
            self.write_op(|st, pager| {
                if let Some(head) = overflow_head(v) {
                    overflow::free_chain(pager, head)?;
                }
                st.sp2o_vlist.delete(pager, k)?;
                Ok(())
            })?;
        }

        // 2. Group the SPO index by (sub, pred); each group's objects are already
        //    sorted ascending and distinct (SPO key order), ready for encoding.
        let all = self.try_iter_spo().map_err(GStoreError::Io)?;
        let mut i = 0;
        while i < all.len() {
            let (s, p) = (all[i].sub, all[i].pred);
            let mut objs = Vec::new();
            while i < all.len() && all[i].sub == s && all[i].pred == p {
                objs.push(all[i].obj);
                i += 1;
            }
            let enc = vlist::encode_u32s(&objs);
            self.write_op(|st, pager| {
                let rec = encode_vlist_record(pager, &enc)?;
                st.sp2o_vlist.insert(pager, &cat(s, p), &rec)
            })?;
        }

        // 3. Publish: reads may now use the VList index.
        self.write_op(|st, pager| {
            st.compacted = true;
            pager.set_root(ROOT_COMPACTED, 1);
            Ok(())
        })
    }

    /// Whether the VList-compressed index is currently valid (set by
    /// [`compact`](Self::compact), cleared by any write). Read from the shared
    /// header, so a read-only view sees its writer's changes at once.
    pub fn is_compacted(&self) -> bool {
        self.read_pager().root(ROOT_COMPACTED) != 0
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
        self.sp2o_vlist
            .get_with(&pager, &cat(s, p), record_logical_len)
    }

    /// Whether the stored VList value for `(s, p)` uses an [`overflow`] page
    /// chain (i.e. its encoding exceeded [`MAX_INLINE_VLIST_BYTES`]). `None` if
    /// the group is absent from the compressed index. Exposes the inline/overflow
    /// decision for tests.
    pub fn compact_value_is_overflow(&self, s: EntityLiteralId, p: PredId) -> Result<Option<bool>> {
        let pager = self.pager.read().unwrap();
        self.sp2o_vlist
            .get_with(&pager, &cat(s, p), |v| v.first() == Some(&OVERFLOW_TAG))
    }

    // ---- bridge to the in-memory engine ----------------------------------

    /// Reconstruct an in-memory [`Dictionary`] + [`TripleStore`] from disk.
    /// Ids are reassigned in the same order, so the reconstructed state is
    /// identical to having built in memory — allowing the existing query engine
    /// (and VS-tree) to run against a disk-built database.
    pub fn to_memory(&self) -> Result<(Dictionary, TripleStore)> {
        let dict = self.dictionary()?;
        let mut store = TripleStore::new();
        store.bulk_load(self.try_iter_spo().map_err(GStoreError::Io)?);
        Ok((dict, store))
    }

    /// Reconstruct just the in-memory [`Dictionary`] (ids in disk order, so they
    /// line up with the on-disk triple indexes). One sequential pass over each
    /// `id2*` array (a directory page serves 32 ids).
    pub fn dictionary(&self) -> Result<Dictionary> {
        let mut dict = Dictionary::new();
        let pager = self.pager.read().unwrap();
        load_terms(&self.id2entity, &pager, self.entity_count, |s| {
            dict.try_intern_entity(s)
        })?;
        load_terms(&self.id2literal, &pager, self.literal_count, |s| {
            dict.try_intern_literal(s)
        })?;
        load_terms(&self.id2predicate, &pager, self.pred_count, |s| {
            dict.try_intern_predicate(s)
        })?;
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

/// Let the query engine evaluate directly against the on-disk store, *streaming*
/// the index ranges each pattern touches through the page cache rather than
/// materializing the whole graph in memory. Every method delegates to the
/// matching `try_*` (so answers come in the trait's contract order); a read
/// error degrades to an empty answer here — the database layer, not this
/// convenience impl, is where read errors are reported per query.
impl TripleSource for DiskStore {
    fn exists(&self, s: u32, p: u32, o: u32) -> bool {
        self.try_exists(s, p, o).unwrap_or(false)
    }
    fn po_by_s(&self, s: u32) -> Vec<(u32, u32)> {
        self.try_po_by_s(s).unwrap_or_default()
    }
    fn o_by_sp(&self, s: u32, p: u32) -> Vec<u32> {
        self.try_o_by_sp(s, p).unwrap_or_default()
    }
    fn p_by_so(&self, s: u32, o: u32) -> Vec<u32> {
        self.try_p_by_so(s, o).unwrap_or_default()
    }
    fn ps_by_o(&self, o: u32) -> Vec<(u32, u32)> {
        self.try_ps_by_o(o).unwrap_or_default()
    }
    fn s_by_po(&self, p: u32, o: u32) -> Vec<u32> {
        self.try_s_by_po(p, o).unwrap_or_default()
    }
    fn so_by_p(&self, p: u32) -> Vec<(u32, u32)> {
        self.try_so_by_p(p).unwrap_or_default()
    }
    fn subs_by_p(&self, p: u32) -> Vec<u32> {
        self.try_subs_by_p(p).unwrap_or_default()
    }
    fn objs_by_p(&self, p: u32) -> Vec<u32> {
        self.try_objs_by_p(p).unwrap_or_default()
    }
    fn subject_keys(&self) -> Vec<u32> {
        self.try_subject_keys().unwrap_or_default()
    }
    fn object_keys(&self) -> Vec<u32> {
        self.try_object_keys().unwrap_or_default()
    }
    fn triple_count(&self) -> u64 {
        DiskStore::triple_count(self)
    }
    // Maintained counters keep every statistic O(1) on the hot planning path,
    // and exact (they track the triples actually present, incrementally).
    fn distinct_subjects(&self) -> usize {
        self.distinct_subject_count()
    }
    fn distinct_objects(&self) -> usize {
        self.distinct_object_count()
    }
    fn num_predicates(&self) -> usize {
        self.nonempty_preds
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
        self.try_iter_all().unwrap_or_default()
    }

    // C1 counts: the inherent versions count a key prefix (or read the
    // maintained statistics) instead of materializing the range. Each returns
    // exactly what the trait default returns over the methods above, and the
    // `has_*` ones answer `true` on a read error.
    fn count_po_by_s(&self, s: u32) -> u64 {
        DiskStore::count_po_by_s(self, s)
    }
    fn count_o_by_sp(&self, s: u32, p: u32) -> u64 {
        DiskStore::count_o_by_sp(self, s, p)
    }
    fn count_p_by_so(&self, s: u32, o: u32) -> u64 {
        DiskStore::count_p_by_so(self, s, o)
    }
    fn count_ps_by_o(&self, o: u32) -> u64 {
        DiskStore::count_ps_by_o(self, o)
    }
    fn count_s_by_po(&self, p: u32, o: u32) -> u64 {
        DiskStore::count_s_by_po(self, p, o)
    }
    fn count_so_by_p(&self, p: u32) -> u64 {
        DiskStore::count_so_by_p(self, p)
    }
    fn distinct_p_of_s(&self, s: u32) -> u64 {
        DiskStore::distinct_p_of_s(self, s)
    }
    fn distinct_p_of_o(&self, o: u32) -> u64 {
        DiskStore::distinct_p_of_o(self, o)
    }
    fn has_subject(&self, s: u32) -> bool {
        DiskStore::has_subject(self, s)
    }
    fn has_object(&self, o: u32) -> bool {
        DiskStore::has_object(self, o)
    }
    fn has_predicate(&self, p: u32) -> bool {
        DiskStore::has_predicate(self, p)
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

/// Intern every string of `arr` in `0..n`, in id order (an absent id is
/// skipped, as the per-id loop this replaces did).
/// An empty `Vec` with room for `hint` elements when the allocator can give
/// it. Hints come from counters in the file (header roots, statistics
/// records), which a damaged file can make arbitrarily large: a reservation
/// that fails is skipped, and the vector grows as it fills, instead of the
/// allocation failure aborting the process.
fn with_hint<T>(hint: u64) -> Vec<T> {
    let mut v = Vec::new();
    if let Ok(n) = usize::try_from(hint) {
        let _ = v.try_reserve_exact(n);
    }
    v
}

fn load_terms(
    arr: &IvArray,
    pager: &Pager,
    n: u32,
    mut intern: impl FnMut(&str) -> Result<u32>,
) -> Result<()> {
    let mut failed = None;
    arr.for_each_range(pager, 0, n, |_, b| {
        match intern(&String::from_utf8_lossy(b)) {
            Ok(_) => ControlFlow::Continue(()),
            Err(e) => {
                failed = Some(e);
                ControlFlow::Break(())
            }
        }
    })?;
    failed.map_or(Ok(()), Err)
}

/// Decode a `(num, distinct_subj, distinct_obj)` stats record.
fn decode_pred_stat(rec: &[u8]) -> Option<PredStat> {
    if rec.len() < PRED_STAT_LEN {
        return None;
    }
    let u = |i: usize| u32::from_le_bytes([rec[i], rec[i + 1], rec[i + 2], rec[i + 3]]);
    Some((u(0), u(4), u(8)))
}

/// Write a predicate's `(num, distinct_subj, distinct_obj)` stats record. A
/// predicate whose triple count has fallen to zero is removed from the array
/// (its distinct counts are zero too), keeping the array sparse over live
/// predicates.
fn write_pred_stat(stats: &IvArray, pager: &mut Pager, pred: PredId, st: PredStat) -> Result<()> {
    let (num, dsub, dobj) = st;
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
pub(crate) fn key3(a: u32, b: u32, c: u32) -> [u8; 12] {
    let mut k = [0u8; 12];
    k[0..4].copy_from_slice(&be32(a));
    k[4..8].copy_from_slice(&be32(b));
    k[8..12].copy_from_slice(&be32(c));
    k
}

/// 16-byte quad key `g|s|p|o` (big-endian, ordering-preserving).
pub(crate) fn key4(g: u32, s: u32, p: u32, o: u32) -> [u8; 16] {
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
        let p = std::env::temp_dir().join(format!("gstore_disk_{tag}_{}.kv", std::process::id()));
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
        // The N-Quads statement keeps its graph: two default-graph triples and
        // one named-graph quad (no longer flattened into the default graph).
        assert_eq!(ds.triple_count(), 2);
        assert_eq!(ds.quad_count(), 1);
        let quads = ds.quads().unwrap();
        let g = ds.id_to_string(quads[0].0).unwrap();
        assert_eq!(g.as_deref(), Some("<http://ex/g>"));
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

    #[test]
    fn id_spaces_are_bounded_like_the_dictionary() {
        use crate::dict::{ENTITY_ID_LIMIT, LITERAL_INDEX_LIMIT, PREDICATE_ID_LIMIT};
        let path = tmp("id_bounds");
        let mut ds = DiskStore::build_str(&path, 64, SMALL).unwrap();
        // Pretend each space is full: a *new* term must be refused (before
        // anything is written), while already-interned terms still resolve.
        ds.entity_count = ENTITY_ID_LIMIT;
        ds.literal_count = LITERAL_INDEX_LIMIT;
        ds.pred_count = PREDICATE_ID_LIMIT;
        let e = ds.intern_entity("<brand-new>").unwrap_err().to_string();
        assert!(e.contains("entity id space exhausted"), "{e}");
        let e = ds.intern_literal("\"brand-new\"").unwrap_err().to_string();
        assert!(e.contains("literal id space exhausted"), "{e}");
        let e = ds.intern_predicate("<brand-new>").unwrap_err().to_string();
        assert!(e.contains("predicate id space exhausted"), "{e}");
        assert!(ds.insert_keys("<root>", "<rel>", "<x>", false).is_err());
        assert_eq!(ds.intern_entity("<root>").unwrap(), 0);
        assert!(ds.intern_predicate("<contain>").is_ok());
        drop(ds);
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(path.with_extension("kv.wal")).ok();
    }

    // ---- Phase 6 R1-KV: contract-ordered try_* reads, exact counts --------

    fn cleanup(path: &std::path::Path) {
        std::fs::remove_file(path).ok();
        std::fs::remove_file(path.with_extension("kv.wal")).ok();
    }

    /// xorshift64*: deterministic data for the equivalence tests.
    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: u64) -> u32 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) % n) as u32
        }
    }

    /// Build a disk store and an in-memory store holding the same id triples:
    /// `n` random inserts (entities, literals and a few predicates, interned
    /// through the store so ids are the store's), then every `del`-th triple
    /// deleted from both.
    fn twin_stores(path: &std::path::Path, seed: u64, n: usize, del: usize) -> (DiskStore, TripleStore) {
        let mut ds = DiskStore::open(path, 64).unwrap();
        let mut mem = TripleStore::new();
        let mut rng = Rng(seed);
        let mut added = Vec::new();
        for _ in 0..n {
            let s = ds.intern_entity(&format!("<s{}>", rng.below(60))).unwrap();
            let p = ds.intern_predicate(&format!("<p{}>", rng.below(7))).unwrap();
            let o = if rng.below(3) == 0 {
                ds.intern_literal(&format!("\"l{}\"", rng.below(40))).unwrap()
            } else {
                // Objects overlap the subjects, so ids are both.
                ds.intern_entity(&format!("<s{}>", rng.below(90))).unwrap()
            };
            let t = IdTriple::new(s, p, o);
            assert_eq!(ds.insert_ids(t).unwrap(), mem.insert(t));
            added.push(t);
        }
        if del > 0 {
            for t in added.iter().step_by(del) {
                assert_eq!(ds.delete_ids(*t).unwrap(), mem.remove(*t));
            }
        }
        (ds, mem)
    }

    /// Every read of `ds` equals (in order) the same read of `mem`.
    fn assert_same_reads(ds: &DiskStore, mem: &TripleStore) {
        let ents: Vec<u32> = (0..ds.entity_num() as u32 + 2).collect();
        let lits: Vec<u32> = (0..ds.literal_num() as u32 + 2)
            .map(|i| LITERAL_FIRST_ID + i)
            .collect();
        let ids: Vec<u32> = ents.iter().chain(&lits).copied().collect();
        let preds: Vec<u32> = (0..ds.predicate_num() as u32 + 2).collect();
        let mem_src: &dyn TripleSource = mem;
        let disk_src: &dyn TripleSource = ds;
        for &s in &ents {
            assert_eq!(ds.try_po_by_s(s).unwrap(), mem.po_by_s(s).to_vec(), "po_by_s({s})");
            assert_eq!(ds.count_po_by_s(s), mem.po_by_s(s).len() as u64);
            assert_eq!(disk_src.po_by_s(s), mem_src.po_by_s(s));
            let distinct_p = {
                let mut v: Vec<u32> = mem.po_by_s(s).iter().map(|x| x.0).collect();
                v.dedup();
                v.len() as u64
            };
            assert_eq!(ds.distinct_p_of_s(s), distinct_p);
            assert_eq!(ds.has_subject(s), !mem.po_by_s(s).is_empty());
            for &p in &preds {
                assert_eq!(ds.try_o_by_sp(s, p).unwrap(), mem.o_by_sp(s, p), "o_by_sp({s},{p})");
                assert_eq!(ds.count_o_by_sp(s, p), mem.o_by_sp(s, p).len() as u64);
            }
            for &o in &ids {
                assert_eq!(ds.try_p_by_so(s, o).unwrap(), mem.p_by_so(s, o), "p_by_so({s},{o})");
                assert_eq!(ds.count_p_by_so(s, o), mem.p_by_so(s, o).len() as u64);
                assert_eq!(ds.try_exists(s, 0, o).unwrap(), mem.exists(s, 0, o));
            }
        }
        for &o in &ids {
            assert_eq!(ds.try_ps_by_o(o).unwrap(), mem.ps_by_o(o).to_vec(), "ps_by_o({o})");
            assert_eq!(ds.count_ps_by_o(o), mem.ps_by_o(o).len() as u64);
            assert_eq!(disk_src.ps_by_o(o), mem_src.ps_by_o(o));
            let mut ps: Vec<u32> = mem.ps_by_o(o).iter().map(|x| x.0).collect();
            ps.dedup();
            assert_eq!(ds.distinct_p_of_o(o), ps.len() as u64);
            assert_eq!(ds.has_object(o), !mem.ps_by_o(o).is_empty());
            for &p in &preds {
                assert_eq!(ds.try_s_by_po(p, o).unwrap(), mem.s_by_po(p, o), "s_by_po({p},{o})");
                assert_eq!(ds.count_s_by_po(p, o), mem.s_by_po(p, o).len() as u64);
            }
        }
        for &p in &preds {
            assert_eq!(ds.try_so_by_p(p).unwrap(), mem.so_by_p(p).to_vec(), "so_by_p({p})");
            assert_eq!(ds.count_so_by_p(p), mem.so_by_p(p).len() as u64);
            assert_eq!(ds.try_subs_by_p(p).unwrap(), mem.subs_by_p(p), "subs_by_p({p})");
            assert_eq!(ds.try_objs_by_p(p).unwrap(), mem.objs_by_p(p), "objs_by_p({p})");
            assert_eq!(disk_src.so_by_p(p), mem_src.so_by_p(p));
            assert_eq!(ds.has_predicate(p), !mem.so_by_p(p).is_empty());
            assert_eq!(disk_src.pred_card(p), mem_src.pred_card(p));
            assert_eq!(disk_src.pred_distinct_subj(p), mem_src.pred_distinct_subj(p));
            assert_eq!(disk_src.pred_distinct_obj(p), mem_src.pred_distinct_obj(p));
        }
        assert_eq!(ds.try_subject_keys().unwrap(), mem.subject_keys().collect::<Vec<_>>());
        assert_eq!(ds.try_object_keys().unwrap(), mem.object_keys().collect::<Vec<_>>());
        assert_eq!(ds.try_iter_all().unwrap(), mem.iter_all().collect::<Vec<_>>());
        assert_eq!(disk_src.iter_all(), mem_src.iter_all());
        let mut spo: Vec<IdTriple> = mem.iter_all().collect();
        spo.sort_unstable_by_key(|t| (t.sub, t.pred, t.obj));
        assert_eq!(ds.try_iter_spo().unwrap(), spo);
        assert_eq!(disk_src.triple_count(), mem_src.triple_count());
        assert_eq!(disk_src.distinct_subjects(), mem_src.distinct_subjects());
        assert_eq!(disk_src.distinct_objects(), mem_src.distinct_objects());
        assert_eq!(disk_src.num_predicates(), mem_src.num_predicates());
        assert_eq!(ds.try_num_predicates().unwrap(), mem.num_predicates());
        assert_c1_overrides_match_defaults(disk_src, mem_src, &ents, &ids, &preds);
    }

    /// The C1 methods of `impl TripleSource for DiskStore` (which override the
    /// trait defaults with prefix counts) return exactly what the defaults
    /// compute over the list methods, and what the in-memory store answers.
    fn assert_c1_overrides_match_defaults(
        disk: &dyn TripleSource,
        mem: &dyn TripleSource,
        ents: &[u32],
        ids: &[u32],
        preds: &[u32],
    ) {
        let runs = |v: &[(u32, u32)]| crate::store::distinct_first(v);
        for &s in ents {
            assert_eq!(disk.count_po_by_s(s), disk.po_by_s(s).len() as u64, "count_po_by_s({s})");
            assert_eq!(disk.count_po_by_s(s), mem.count_po_by_s(s));
            assert_eq!(disk.distinct_p_of_s(s), runs(&disk.po_by_s(s)), "distinct_p_of_s({s})");
            assert_eq!(disk.distinct_p_of_s(s), mem.distinct_p_of_s(s));
            assert_eq!(disk.has_subject(s), !disk.po_by_s(s).is_empty(), "has_subject({s})");
            assert_eq!(disk.has_subject(s), mem.has_subject(s));
            for &p in preds {
                assert_eq!(disk.count_o_by_sp(s, p), disk.o_by_sp(s, p).len() as u64);
                assert_eq!(disk.count_o_by_sp(s, p), mem.count_o_by_sp(s, p));
            }
            for &o in ids {
                assert_eq!(disk.count_p_by_so(s, o), disk.p_by_so(s, o).len() as u64);
                assert_eq!(disk.count_p_by_so(s, o), mem.count_p_by_so(s, o));
            }
        }
        for &o in ids {
            assert_eq!(disk.count_ps_by_o(o), disk.ps_by_o(o).len() as u64, "count_ps_by_o({o})");
            assert_eq!(disk.count_ps_by_o(o), mem.count_ps_by_o(o));
            assert_eq!(disk.distinct_p_of_o(o), runs(&disk.ps_by_o(o)), "distinct_p_of_o({o})");
            assert_eq!(disk.distinct_p_of_o(o), mem.distinct_p_of_o(o));
            assert_eq!(disk.has_object(o), !disk.ps_by_o(o).is_empty(), "has_object({o})");
            assert_eq!(disk.has_object(o), mem.has_object(o));
            for &p in preds {
                assert_eq!(disk.count_s_by_po(p, o), disk.s_by_po(p, o).len() as u64);
                assert_eq!(disk.count_s_by_po(p, o), mem.count_s_by_po(p, o));
            }
        }
        for &p in preds {
            assert_eq!(disk.count_so_by_p(p), disk.so_by_p(p).len() as u64, "count_so_by_p({p})");
            assert_eq!(disk.count_so_by_p(p), mem.count_so_by_p(p));
            assert_eq!(disk.has_predicate(p), !disk.so_by_p(p).is_empty(), "has_predicate({p})");
            assert_eq!(disk.has_predicate(p), mem.has_predicate(p));
        }
    }

    #[test]
    fn try_reads_equal_triple_store_in_order() {
        for (seed, n, del) in [(1u64, 0usize, 0usize), (2, 1, 0), (3, 400, 0), (4, 900, 3), (5, 700, 1)] {
            let path = tmp(&format!("twin_{seed}"));
            let (ds, mem) = twin_stores(&path, seed, n, del);
            assert_same_reads(&ds, &mem);
            // The same after a commit and a reopen (the stats mirror reloads).
            ds.commit().unwrap();
            drop(ds);
            let ds = DiskStore::open(&path, 64).unwrap();
            assert_same_reads(&ds, &mem);
            cleanup(&path);
        }
    }

    #[test]
    fn exact_counts_after_deleting_whole_predicates_and_subjects() {
        let path = tmp("exact_counts");
        let (mut ds, mut mem) = twin_stores(&path, 9, 500, 0);
        // Remove every triple of predicate 0 and of subject 0.
        let doomed: Vec<IdTriple> = mem
            .iter_all()
            .filter(|t| t.pred == 0 || t.sub == 0)
            .collect();
        for t in doomed {
            assert!(ds.delete_ids(t).unwrap());
            assert!(mem.remove(t));
        }
        assert_eq!(ds.distinct_subject_count(), mem.distinct_subjects());
        assert_eq!(TripleSource::num_predicates(&ds), mem.num_predicates());
        assert!(TripleSource::num_predicates(&ds) < ds.predicate_num());
        assert!(!ds.has_predicate(0) && !ds.has_subject(0));
        assert_same_reads(&ds, &mem);
        // Empty store: zero, not one.
        let path2 = tmp("exact_counts_empty");
        let empty = DiskStore::open(&path2, 16).unwrap();
        assert_eq!(TripleSource::distinct_subjects(&empty), 0);
        assert_eq!(TripleSource::num_predicates(&empty), 0);
        cleanup(&path);
        cleanup(&path2);
    }

    #[test]
    fn entity_adjacency_pass_matches_per_entity_reads() {
        let path = tmp("adjacency");
        let (ds, mem) = twin_stores(&path, 21, 800, 4);
        // The reference: what `vstree_entries` reads per entity.
        let mut ids: Vec<u32> = mem.subject_keys().collect();
        ids.extend(mem.object_keys().filter(|&o| !is_literal_id(o)));
        ids.sort_unstable();
        ids.dedup();
        let mut seen = Vec::new();
        ds.for_each_entity_adjacency(|e, out, inn| {
            assert_eq!(out, mem.po_by_s(e), "out edges of {e}");
            assert_eq!(inn, mem.ps_by_o(e), "in edges of {e}");
            seen.push(e);
            ControlFlow::Continue(())
        })
        .unwrap();
        assert_eq!(seen, ids);
        let mut n = 0;
        ds.for_each_entity_adjacency(|_, _, _| {
            n += 1;
            if n == 3 {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        })
        .unwrap();
        assert_eq!(n, 3);
        cleanup(&path);
    }

    #[test]
    fn a_failed_write_leaves_no_trace() {
        let path = tmp("atomic_fail");
        let mut ds = DiskStore::build_str(&path, 64, SMALL).unwrap();
        let (ents, preds) = (ds.entity_num(), ds.predicate_num());
        let pages = ds.page_count_for_test();
        // The predicate space is full, so the insert fails *after* interning
        // its new subject: the whole operation is rolled back.
        ds.pred_count = crate::dict::PREDICATE_ID_LIMIT;
        assert!(ds.insert_keys("<brand-new>", "<rel>", "<x>", false).is_err());
        ds.pred_count = preds as u32;
        assert_eq!(ds.entity_num(), ents, "the subject term was rolled back");
        assert_eq!(ds.term_id(&Term::iri("brand-new")).unwrap(), None);
        assert_eq!(ds.page_count_for_test(), pages);
        assert_eq!(ds.triple_count(), 5);
        // A key too large for a page fails the same way, after the subject.
        let huge = format!("<{}>", "z".repeat(PAGE_SIZE));
        assert!(ds.insert_keys("<brand-new>", "<rel>", &huge, false).is_err());
        assert_eq!(ds.entity_num(), ents);
        // The store stays usable and consistent.
        assert!(ds.insert_keys("<brand-new>", "<rel>", "<x>", false).unwrap());
        assert_eq!(ds.entity_num(), ents + 2);
        ds.flush().unwrap();
        drop(ds);
        let ds = DiskStore::open(&path, 64).unwrap();
        assert_eq!(ds.entity_num(), ents + 2);
        assert_eq!(ds.triple_count(), 6);
        let (dict, mem) = ds.to_memory().unwrap();
        assert_eq!(dict.entity_num(), ents + 2);
        assert_eq!(mem.triple_count(), 6);
        cleanup(&path);
    }

    #[test]
    fn large_terms_split_pages_by_bytes_and_keep_the_store_usable() {
        // Phase 6 R1 review: three small literals, then three of 1.4 KB, used
        // to split a literal2id leaf at its middle entry, past the page: a
        // panic that also poisoned the shared pager. Subjects of 2 KB did the
        // same in entity2id.
        let path = tmp("large_terms");
        let mut ds = DiskStore::open(&path, 64).unwrap();
        let mut lits: Vec<String> = ["\"a\"", "\"b\"", "\"c\""].map(String::from).to_vec();
        lits.extend((b'x'..=b'z').map(|c| format!("\"{}\"", (c as char).to_string().repeat(1400))));
        for (i, lit) in lits.iter().enumerate() {
            assert!(ds
                .insert_keys(&format!("<s{}>", i % 3), "<p>", lit, true)
                .unwrap());
        }
        let long = |c: char| {
            format!(
                "<{}>",
                c.to_string().repeat(super::super::bptree::MAX_KEY_LEN - 2)
            )
        };
        for s in ["<a>".to_string(), "<b>".to_string(), long('y'), long('z')] {
            assert!(ds.insert_keys(&s, "<q>", "<o>", false).unwrap());
        }
        // A key over the limit is refused cleanly, and nothing is poisoned.
        let too_long = format!("<{}>", "w".repeat(super::super::bptree::MAX_KEY_LEN));
        assert!(ds.insert_keys(&too_long, "<q>", "<o>", false).is_err());
        assert_eq!(ds.try_iter_spo().unwrap().len(), 10);
        ds.flush().unwrap();
        drop(ds);
        let ds = DiskStore::open(&path, 64).unwrap();
        assert_eq!(ds.triple_count(), 10);
        let (dict, mem) = ds.to_memory().unwrap();
        assert_eq!(mem.triple_count(), 10);
        for lit in &lits {
            assert!(dict.literal_id(lit).is_some(), "{} bytes", lit.len());
        }
        assert!(dict.entity_id(&long('z')).is_some());
        assert!(dict.entity_id(&too_long).is_none());
        cleanup(&path);
    }

    #[test]
    fn a_panicking_write_is_rolled_back_without_poisoning_the_pager() {
        let path = tmp("write_panic");
        let mut ds = DiskStore::build_str(&path, 64, SMALL).unwrap();
        let view = DiskStore::from_shared_pager(ds.shared_pager()).unwrap();
        let (ents, pages) = (ds.entity_num(), ds.page_count_for_test());
        let r: Result<()> = ds.write_op(|st, pg| {
            st.intern_entity_in(pg, "<half-done>")?;
            panic!("a bug inside a write");
        });
        let err = r.unwrap_err().to_string();
        assert!(err.contains("a bug inside a write"), "{err}");
        assert_eq!(ds.entity_num(), ents);
        assert_eq!(ds.page_count_for_test(), pages);
        assert_eq!(ds.term_id(&Term::iri("half-done")).unwrap(), None);
        // Readers and writers sharing the pager carry on.
        assert_eq!(view.try_iter_spo().unwrap().len(), 5);
        assert!(ds
            .insert_keys("<after>", "<own>", "<point0>", false)
            .unwrap());
        assert_eq!(ds.try_iter_spo().unwrap().len(), 6);
        cleanup(&path);
    }

    #[test]
    fn a_logical_insert_is_one_commit_unit_even_with_a_tiny_cache() {
        // With an 8-page cache, a single insert touches more pages than fit;
        // the operation must still never be committed in part, so the file only
        // ever holds whole triples. We check that every commit point (flushes
        // forced by later operations) leaves the three indexes in agreement.
        let path = tmp("atomic_tiny_cache");
        let mut ds = DiskStore::open(&path, 8).unwrap();
        for i in 0..300u32 {
            ds.insert_keys(
                &format!("<http://ex/subject/with/a/long/iri/{i}>"),
                &format!("<http://ex/p{}>", i % 5),
                &format!("\"literal value number {i}\""),
                true,
            )
            .unwrap();
            if i % 37 == 0 {
                // Open the committed file alone: it must be self-consistent.
                let snap = DiskStore::open(&path, 64).unwrap();
                let spo = snap.try_iter_spo().unwrap();
                assert_eq!(spo.len() as u64, snap.triple_count());
                let mut pos = 0;
                for p in 0..snap.predicate_num() as u32 {
                    pos += snap.try_so_by_p(p).unwrap().len();
                }
                assert_eq!(pos, spo.len(), "POS agrees with SPO at a commit point");
                assert_eq!(snap.try_object_keys().unwrap().len() as u64, snap.triple_count());
                let (dict, _) = snap.to_memory().unwrap();
                assert_eq!(dict.entity_num(), snap.entity_num());
            }
        }
        cleanup(&path);
    }

    #[test]
    fn read_only_view_shares_the_pager() {
        let path = tmp("view");
        let mut ds = DiskStore::build_str(&path, 64, SMALL).unwrap();
        let mut view = DiskStore::from_shared_pager(ds.shared_pager()).unwrap();
        assert!(view.is_read_only());
        assert_eq!(view.triple_count(), 5);
        assert_eq!(view.try_iter_all().unwrap(), ds.try_iter_all().unwrap());
        assert_eq!(TripleSource::num_predicates(&view), 3);
        assert!(view.insert_keys("<a>", "<b>", "<c>", false).is_err());
        assert!(view.flush().is_err() && view.commit().is_err());
        // The writer goes on; the view's counters catch up on refresh, its
        // index reads see the shared pages at once.
        assert!(ds.insert_keys("<a>", "<b>", "<c>", false).unwrap());
        assert_eq!(view.triple_count(), 5);
        assert_eq!(view.try_iter_spo().unwrap().len(), 6);
        view.refresh().unwrap();
        assert_eq!(view.triple_count(), 6);
        assert_eq!(TripleSource::num_predicates(&view), 4);
        ds.commit().unwrap();
        cleanup(&path);
    }

    #[test]
    fn a_view_reads_the_vlist_index_only_while_it_is_current() {
        // Phase 6 R1 review: a view built after `compact` kept its own copy of
        // the compacted flag, so once the writer changed a group its `o_by_sp`
        // answered from the outdated VList while its other reads were live.
        let path = tmp("view_compacted");
        let mut ds =
            DiskStore::build_str(&path, 64, "<a> <p> <b> .\n<a> <p> <c> .\n<a> <q> <d> .\n")
                .unwrap();
        ds.compact().unwrap();
        let view = DiskStore::from_shared_pager(ds.shared_pager()).unwrap();
        assert!(view.is_compacted());
        let a = ds.term_id(&Term::iri("a")).unwrap().unwrap();
        let p = ds.predicate_id("<p>").unwrap().unwrap();
        assert_eq!(
            view.try_o_by_sp(a, p).unwrap(),
            ds.try_o_by_sp(a, p).unwrap()
        );
        assert!(ds.insert_keys("<a>", "<p>", "<e>", false).unwrap());
        assert!(!view.is_compacted());
        let live = ds.try_o_by_sp(a, p).unwrap();
        assert_eq!(live.len(), 3);
        assert_eq!(view.try_o_by_sp(a, p).unwrap(), live);
        assert!(ds.delete_keys("<a>", "<p>", "<b>", false).unwrap());
        assert_eq!(
            view.try_o_by_sp(a, p).unwrap(),
            ds.try_o_by_sp(a, p).unwrap()
        );
        // Compacted again: the view uses the new index at once.
        ds.compact().unwrap();
        assert!(view.is_compacted());
        assert_eq!(
            view.try_o_by_sp(a, p).unwrap(),
            ds.try_o_by_sp(a, p).unwrap()
        );
        cleanup(&path);
    }

    /// Overwrite header root slot `slot` of the file at `path` with `v`.
    fn poke_root(path: &std::path::Path, slot: usize, v: u64) {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.seek(SeekFrom::Start(16 + 8 * slot as u64)).unwrap();
        f.write_all(&v.to_le_bytes()).unwrap();
    }

    #[test]
    fn corrupt_header_counters_are_errors_not_aborts() {
        // Phase 6 R1 review: `open` sized allocations from header counters, so
        // a damaged count aborted the process (allocation failure) instead of
        // failing the open.
        let path = tmp("corrupt_counts");
        DiskStore::build_str(&path, 64, SMALL)
            .unwrap()
            .flush()
            .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        for slot in [ROOT_PRED_COUNT, ROOT_ENTITY_COUNT, ROOT_LITERAL_COUNT] {
            for v in [u64::from(u32::MAX), 1 << 40] {
                std::fs::write(&path, &bytes).unwrap();
                poke_root(&path, slot, v);
                let err = DiskStore::open(&path, 64).err().expect("a corrupt count");
                assert!(err.to_string().contains("corrupt"), "{err}");
            }
        }
        // Counters that only size a buffer: the open and the reads still work
        // (the count itself is what the file says).
        for slot in [
            ROOT_TRIPLE_COUNT,
            ROOT_QUAD_COUNT,
            ROOT_DISTINCT_SUBJ,
            ROOT_DISTINCT_OBJ,
        ] {
            std::fs::write(&path, &bytes).unwrap();
            poke_root(&path, slot, 1 << 44);
            let ds = DiskStore::open(&path, 64).unwrap();
            assert_eq!(ds.try_iter_spo().unwrap().len(), 5);
            assert_eq!(ds.try_subject_keys().unwrap().len(), 2);
            assert_eq!(ds.try_object_keys().unwrap().len(), 5);
            assert!(ds.quads().unwrap().is_empty());
            let (_, mem) = ds.to_memory().unwrap();
            assert_eq!(mem.triple_count(), 5);
        }
        cleanup(&path);
    }

    #[test]
    fn dictionary_bulk_load_matches_per_id_resolution() {
        let path = tmp("dict_bulk");
        let mut content = String::new();
        for i in 0..250u32 {
            content.push_str(&format!(
                "<http://ex/s{i}> <http://ex/p{}> \"{}\" .\n",
                i % 3,
                "v".repeat(i as usize % 200)
            ));
        }
        let ds = DiskStore::build_str(&path, 32, &content).unwrap();
        let dict = ds.dictionary().unwrap();
        for id in 0..ds.entity_num() as u32 {
            assert_eq!(ds.id_to_string(id).unwrap().as_deref(), dict.id_to_string(id));
        }
        for i in 0..ds.literal_num() as u32 {
            let id = LITERAL_FIRST_ID + i;
            assert_eq!(ds.id_to_string(id).unwrap().as_deref(), dict.id_to_string(id));
        }
        for p in 0..ds.predicate_num() as u32 {
            assert_eq!(ds.predicate_to_string(p).unwrap().as_deref(), dict.predicate_to_string(p));
        }
        cleanup(&path);
    }

    #[test]
    fn a_store_written_by_this_build_keeps_the_v1_header() {
        let path = tmp("v1_header");
        let ds = DiskStore::build_str(&path, 64, SMALL).unwrap();
        drop(ds);
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..8], super::super::format::MAGIC_V1);
        assert!(bytes[16 + 24 * 8..PAGE_SIZE].iter().all(|&b| b == 0));
        cleanup(&path);
    }
}
