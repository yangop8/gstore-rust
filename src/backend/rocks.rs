//! A persistent RocksDB-backed triple store.
//!
//! Mirrors the in-memory [`TripleStore`](crate::store::TripleStore): the same
//! three redundant orderings answer every triple-pattern shape, but the data
//! lives on disk in RocksDB column families and survives process restarts.
//!
//! | column family | composite key (big-endian) | answers                          |
//! |---------------|----------------------------|----------------------------------|
//! | `spo`         | `s ‖ p ‖ o`                | `exists`, `po_by_s`, `o_by_sp`   |
//! | `pos`         | `p ‖ o ‖ s`                | `s_by_po`, `so_by_p`, `*_by_p`   |
//! | `osp`         | `o ‖ s ‖ p`                | `ps_by_o`, `p_by_so`, `object_keys` |
//! | `stats`       | tagged counter keys        | optimizer statistics             |
//! | `dict`        | `space ‖ id`               | the string dictionary (`id → key`) |
//! | `gspo`        | `g ‖ s ‖ p ‖ o`            | named-graph quads                |
//! | `meta`        | UTF-8 name                 | database name, layout markers    |
//!
//! Each id component is a 4-byte **big-endian** `u32`, so RocksDB's byte order
//! equals numeric order and a prefix range scan over an ordering returns exactly
//! the matching triples, already sorted — the disk analogue of `TripleStore`'s
//! sorted adjacency vectors. Values are empty: the key *is* the triple.
//!
//! ## Optimizer statistics: maintained counters (the `stats` CF)
//!
//! The cost-based optimizer calls `triple_count` / `distinct_subjects` /
//! `distinct_objects` / `num_predicates` / `pred_card` / `pred_distinct_subj` /
//! `pred_distinct_obj` on the hot planning path, so they must be **O(1)**, not
//! range scans. We therefore *maintain* them by reference counting on every
//! insert/remove, persisted in the `stats` CF (and cached in memory for the four
//! global ones). Each logical statistic is backed by a tagged counter key:
//!
//! | tag            | key bytes        | meaning                                   |
//! |----------------|------------------|-------------------------------------------|
//! | `T`/`P`/`S`/`O`| 1 byte           | triples / distinct preds / subjects / objs (global) |
//! | `c` + `p`      | 5 bytes          | `pred_card(p)`                            |
//! | `u` + `p`      | 5 bytes          | `pred_distinct_subj(p)`                    |
//! | `v` + `p`      | 5 bytes          | `pred_distinct_obj(p)`                     |
//! | `s` + `s`      | 5 bytes          | per-subject triple count (transition aid) |
//! | `o` + `o`      | 5 bytes          | per-object triple count (transition aid)  |
//! | `x` + `p` + `s`| 9 bytes          | per-`(pred,subj)` count (transition aid)  |
//! | `y` + `p` + `o`| 9 bytes          | per-`(pred,obj)` count (transition aid)   |
//!
//! A distinct-count is bumped only when its underlying per-key counter crosses
//! `0 ↔ 1`; counters that hit `0` are deleted, so `subject_keys`/`object_keys`/
//! `predicates` can be read by scanning the relevant `stats` prefix. This trades
//! a handful of extra `stats` writes per triple for O(1), *exact* statistics
//! (unlike the disk B+tree store, whose counter-based stats are estimates).
//!
//! ## Durability of the dictionary and named graphs
//!
//! A RocksDB-backed [`Database`](crate::Database) keeps its whole persistent
//! state in this one RocksDB instance: besides the triple orderings, the `dict`
//! CF holds every dictionary term (`E`/`L`/`P` space tag + 4-byte big-endian
//! id → the term's dictionary key) and the `gspo` CF holds the named-graph
//! quads. New terms are written *before* the first triple that references them
//! (an orphan term is harmless; a triple whose term is missing is not), so the
//! state RocksDB recovers after a crash is always self-consistent and nothing
//! depends on a separately-saved `dict.bin`.
//!
//! ## Write errors
//!
//! [`MutableStore`] returns a plain `bool`, so a failed RocksDB write cannot be
//! returned from `insert`/`remove`. Instead the write is reported as "not
//! applied" (`false`), the in-memory counters are left untouched, and the error
//! is kept until [`RocksStore::take_error`] collects it — the `Database` facade
//! surfaces it at its next commit point. Nothing panics on an I/O failure.

use std::path::{Path, PathBuf};

use rocksdb::{
    checkpoint::Checkpoint, ColumnFamilyDescriptor, DBCompressionType, Direction, IteratorMode,
    Options, WriteBatch, DB,
};

use crate::error::{GStoreError, Result};
use crate::model::id::{EntityLiteralId, PredId};
use crate::model::IdTriple;
use crate::store::{MutableStore, TripleSource};

const CF_SPO: &str = "spo";
const CF_POS: &str = "pos";
const CF_OSP: &str = "osp";
const CF_STATS: &str = "stats";
const CF_DICT: &str = "dict";
const CF_GSPO: &str = "gspo";
const CF_META: &str = "meta";

/// Which id space a dictionary term belongs to (the `dict` CF key tag).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TermSpace {
    /// IRIs and blank nodes (ids `< LITERAL_FIRST_ID`).
    Entity,
    /// Literals (public ids `>= LITERAL_FIRST_ID`).
    Literal,
    /// Predicates (their own id space).
    Predicate,
}

impl TermSpace {
    fn tag(self) -> u8 {
        match self {
            TermSpace::Entity => b'E',
            TermSpace::Literal => b'L',
            TermSpace::Predicate => b'P',
        }
    }

    fn from_tag(tag: u8) -> Option<TermSpace> {
        match tag {
            b'E' => Some(TermSpace::Entity),
            b'L' => Some(TermSpace::Literal),
            b'P' => Some(TermSpace::Predicate),
            _ => None,
        }
    }
}

// Global counter keys (single byte).
const K_TRIPLES: &[u8] = b"T";
const K_PREDS: &[u8] = b"P";
const K_SUBJECTS: &[u8] = b"S";
const K_OBJECTS: &[u8] = b"O";

// Tagged per-key counter prefixes.
const T_CARD: u8 = b'c'; // c|p          -> pred_card(p)
const T_PSUBJ: u8 = b'u'; // u|p         -> pred_distinct_subj(p)
const T_POBJ: u8 = b'v'; // v|p          -> pred_distinct_obj(p)
const T_SUB: u8 = b's'; // s|s          -> per-subject count
const T_OBJ: u8 = b'o'; // o|o          -> per-object count
const T_PS: u8 = b'x'; // x|p|s         -> per-(pred,subj) count
const T_PO: u8 = b'y'; // y|p|o         -> per-(pred,obj) count

/// A persistent, RocksDB-backed triple store. Implements the same
/// [`TripleSource`] + [`MutableStore`] seam as [`TripleStore`](crate::store::TripleStore),
/// so it is a drop-in [`StorageBackend`](crate::store::StorageBackend).
pub struct RocksStore {
    db: DB,
    // In-memory caches of the four global counters (reloaded from the `stats` CF
    // on open) so the hot read path never touches disk for them.
    triple_count: u64,
    num_predicates: u64,
    distinct_subjects: u64,
    distinct_objects: u64,
    /// The first write failure not yet collected by [`take_error`](Self::take_error).
    error: std::sync::Mutex<Option<String>>,
}

impl std::fmt::Debug for RocksStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RocksStore")
            .field("path", &self.db.path())
            .field("triple_count", &self.triple_count)
            .finish()
    }
}

impl RocksStore {
    /// Open (creating if absent) a RocksDB triple store at directory `path`.
    /// Persisted data and maintained statistics are reloaded.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<RocksStore> {
        let mut db_opts = Options::default();
        db_opts.create_if_missing(true);
        db_opts.create_missing_column_families(true);

        let cfs = vec![
            ColumnFamilyDescriptor::new(CF_SPO, triple_cf_opts()),
            ColumnFamilyDescriptor::new(CF_POS, triple_cf_opts()),
            ColumnFamilyDescriptor::new(CF_OSP, triple_cf_opts()),
            ColumnFamilyDescriptor::new(CF_STATS, Options::default()),
            ColumnFamilyDescriptor::new(CF_DICT, Options::default()),
            ColumnFamilyDescriptor::new(CF_GSPO, triple_cf_opts()),
            ColumnFamilyDescriptor::new(CF_META, Options::default()),
        ];
        let db = DB::open_cf_descriptors(&db_opts, path, cfs)
            .map_err(|e| GStoreError::Database(format!("rocksdb open: {e}")))?;

        let mut store = RocksStore {
            db,
            triple_count: 0,
            num_predicates: 0,
            distinct_subjects: 0,
            distinct_objects: 0,
            error: std::sync::Mutex::new(None),
        };
        store.triple_count = store.get_u64(K_TRIPLES);
        store.num_predicates = store.get_u64(K_PREDS);
        store.distinct_subjects = store.get_u64(K_SUBJECTS);
        store.distinct_objects = store.get_u64(K_OBJECTS);
        Ok(store)
    }

    /// Flush memtables to SST so all data is durable on disk (every column
    /// family: triples, statistics, dictionary, named graphs and metadata).
    pub fn flush(&self) -> Result<()> {
        for name in [CF_SPO, CF_POS, CF_OSP, CF_STATS, CF_DICT, CF_GSPO, CF_META] {
            self.db
                .flush_cf(self.cf(name))
                .map_err(|e| GStoreError::Database(format!("rocksdb flush: {e}")))?;
        }
        Ok(())
    }

    /// Fsync RocksDB's write-ahead log: every write acknowledged so far survives
    /// a power failure, not only a process crash. Cheaper than [`flush`](Self::flush)
    /// (no memtable → SST compaction); the `Database` facade calls it at each
    /// commit point.
    pub fn sync_wal(&self) -> Result<()> {
        self.db
            .flush_wal(true)
            .map_err(|e| GStoreError::Database(format!("rocksdb WAL sync: {e}")))
    }

    /// The directory this RocksDB store lives in.
    pub fn path(&self) -> PathBuf {
        self.db.path().to_path_buf()
    }

    /// Write a consistent, openable point-in-time copy of the whole store (all
    /// column families) into the **new** directory `dest` via a RocksDB
    /// checkpoint (hard-linked SST files where possible). `dest` must not exist.
    pub fn checkpoint_to<P: AsRef<Path>>(&self, dest: P) -> Result<()> {
        let cp = Checkpoint::new(&self.db)
            .map_err(|e| GStoreError::Database(format!("rocksdb checkpoint: {e}")))?;
        cp.create_checkpoint(dest.as_ref())
            .map_err(|e| GStoreError::Database(format!("rocksdb checkpoint: {e}")))
    }

    /// Collect (and clear) the first write error recorded since the last call.
    /// `insert`/`remove` report a failed write as "not applied" and stash the
    /// error here instead of panicking.
    pub fn take_error(&self) -> Option<String> {
        self.error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    fn record_error(&self, what: &str, e: rocksdb::Error) {
        let mut slot = self
            .error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if slot.is_none() {
            *slot = Some(format!("rocksdb write ({what}): {e}"));
        }
    }

    // ---- dictionary (dict CF) ---------------------------------------------

    /// Persist dictionary terms `(space, id, key)` in one atomic batch
    /// (overwriting any previous term stored under the same id).
    pub fn put_terms(&self, terms: &[(TermSpace, u32, &str)]) -> Result<()> {
        if terms.is_empty() {
            return Ok(());
        }
        let cf = self.cf(CF_DICT);
        let mut wb = WriteBatch::default();
        for (space, id, key) in terms {
            wb.put_cf(cf, tag1(space.tag(), *id), key.as_bytes());
        }
        self.db
            .write(wb)
            .map_err(|e| GStoreError::Database(format!("rocksdb write (dictionary): {e}")))
    }

    /// Replace the entire stored dictionary with `terms`, atomically (used after
    /// id reclamation and when migrating a legacy `dict.bin`).
    pub fn replace_dictionary<'a, I>(&self, terms: I) -> Result<()>
    where
        I: IntoIterator<Item = (TermSpace, u32, &'a str)>,
    {
        let cf = self.cf(CF_DICT);
        let mut wb = WriteBatch::default();
        // Every dict key is 5 bytes `tag ‖ be(id)`; this range covers them all.
        wb.delete_range_cf(cf, vec![0u8; 5], vec![0xFFu8; 6]);
        for (space, id, key) in terms {
            wb.put_cf(cf, tag1(space.tag(), id), key.as_bytes());
        }
        self.db
            .write(wb)
            .map_err(|e| GStoreError::Database(format!("rocksdb write (dictionary): {e}")))
    }

    /// Every stored dictionary term as `(space, id, key)`, in `(space, id)` order.
    pub fn load_terms(&self) -> Result<Vec<(TermSpace, u32, String)>> {
        let mut out = Vec::new();
        for item in self.db.iterator_cf(self.cf(CF_DICT), IteratorMode::Start) {
            let (k, v) =
                item.map_err(|e| GStoreError::Database(format!("rocksdb read (dictionary): {e}")))?;
            let space = (k.len() == 5)
                .then(|| TermSpace::from_tag(k[0]))
                .flatten()
                .ok_or_else(|| GStoreError::Database("corrupt rocksdb dictionary key".into()))?;
            let key = String::from_utf8(v.to_vec())
                .map_err(|_| GStoreError::Database("non-UTF-8 rocksdb dictionary term".into()))?;
            out.push((space, de(&k[1..5]), key));
        }
        Ok(out)
    }

    // ---- named graphs (gspo CF) --------------------------------------------

    /// Insert a quad (graph-IRI entity id + triple). Returns `true` if new.
    pub fn insert_quad(&self, g: u32, t: IdTriple) -> Result<bool> {
        let cf = self.cf(CF_GSPO);
        let key = key4(g, t.sub, t.pred, t.obj);
        let present = self
            .db
            .get_cf(cf, key)
            .map_err(|e| GStoreError::Database(format!("rocksdb read (quad): {e}")))?
            .is_some();
        if present {
            return Ok(false);
        }
        self.db
            .put_cf(cf, key, b"")
            .map_err(|e| GStoreError::Database(format!("rocksdb write (quad): {e}")))?;
        Ok(true)
    }

    /// Remove a quad. Returns `true` if it existed.
    pub fn remove_quad(&self, g: u32, t: IdTriple) -> Result<bool> {
        let cf = self.cf(CF_GSPO);
        let key = key4(g, t.sub, t.pred, t.obj);
        let present = self
            .db
            .get_cf(cf, key)
            .map_err(|e| GStoreError::Database(format!("rocksdb read (quad): {e}")))?
            .is_some();
        if !present {
            return Ok(false);
        }
        self.db
            .delete_cf(cf, key)
            .map_err(|e| GStoreError::Database(format!("rocksdb write (quad): {e}")))?;
        Ok(true)
    }

    /// Every named-graph quad as `(graph_id, triple)`, sorted.
    pub fn quads(&self) -> Result<Vec<(u32, IdTriple)>> {
        let mut out = Vec::new();
        for item in self.db.iterator_cf(self.cf(CF_GSPO), IteratorMode::Start) {
            let (k, _) =
                item.map_err(|e| GStoreError::Database(format!("rocksdb read (quads): {e}")))?;
            if k.len() != 16 {
                return Err(GStoreError::Database("corrupt rocksdb quad key".into()));
            }
            out.push((
                de(&k[0..4]),
                IdTriple::new(de(&k[4..8]), de(&k[8..12]), de(&k[12..16])),
            ));
        }
        Ok(out)
    }

    // ---- metadata (meta CF) ------------------------------------------------

    /// Store a metadata value (database name, layout markers).
    pub fn put_meta(&self, key: &str, value: &[u8]) -> Result<()> {
        self.db
            .put_cf(self.cf(CF_META), key.as_bytes(), value)
            .map_err(|e| GStoreError::Database(format!("rocksdb write (meta): {e}")))
    }

    /// Read a metadata value.
    pub fn get_meta(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.db
            .get_cf(self.cf(CF_META), key.as_bytes())
            .map_err(|e| GStoreError::Database(format!("rocksdb read (meta): {e}")))
    }

    // ---- column-family handles -------------------------------------------

    fn cf(&self, name: &str) -> &rocksdb::ColumnFamily {
        self.db
            .cf_handle(name)
            .expect("column family was created at open")
    }

    // ---- counter helpers (stats CF) --------------------------------------

    fn get_u64(&self, key: &[u8]) -> u64 {
        self.db
            .get_cf(self.cf(CF_STATS), key)
            .ok()
            .flatten()
            .map(|v| {
                let mut b = [0u8; 8];
                b.copy_from_slice(&v[..8]);
                u64::from_be_bytes(b)
            })
            .unwrap_or(0)
    }

    /// Apply a `+1`/`-1` delta to the counter at `key`, deleting it when it
    /// reaches zero. Returns `(old, new)` so callers can detect `0 ↔ 1`
    /// transitions that move a distinct-count.
    fn step(&self, wb: &mut WriteBatch, key: &[u8], up: bool) -> (u64, u64) {
        let old = self.get_u64(key);
        let new = if up { old + 1 } else { old.saturating_sub(1) };
        let cf = self.cf(CF_STATS);
        if new == 0 {
            wb.delete_cf(cf, key);
        } else {
            wb.put_cf(cf, key, new.to_be_bytes());
        }
        (old, new)
    }

    fn set_global(&self, wb: &mut WriteBatch, key: &[u8], value: u64) {
        wb.put_cf(self.cf(CF_STATS), key, value.to_be_bytes());
    }

    // ---- scans -----------------------------------------------------------

    /// Collect every key in column family `cf_name` starting with `prefix`
    /// (empty `prefix` ⇒ the whole CF). Big-endian keys keep the matches
    /// contiguous and ascending, so this is the disk analogue of a sorted scan.
    fn scan_keys(&self, cf_name: &str, prefix: &[u8]) -> Vec<Box<[u8]>> {
        let cf = self.cf(cf_name);
        let mode = if prefix.is_empty() {
            IteratorMode::Start
        } else {
            IteratorMode::From(prefix, Direction::Forward)
        };
        let mut out = Vec::new();
        for item in self.db.iterator_cf(cf, mode) {
            let (k, _v) = match item {
                Ok(kv) => kv,
                Err(_) => break,
            };
            if !k.starts_with(prefix) {
                break;
            }
            out.push(k);
        }
        out
    }

    /// Distinct entity ids carried by a tagged single-id `stats` prefix
    /// (`s`/`o`): each live counter key is `tag ‖ be(id)`.
    fn ids_under(&self, tag: u8) -> Vec<EntityLiteralId> {
        self.scan_keys(CF_STATS, &[tag])
            .iter()
            .map(|k| de(&k[1..5]))
            .collect()
    }

    /// All predicate ids currently present (one `c`-tagged `stats` key each).
    pub fn predicates(&self) -> Vec<PredId> {
        self.scan_keys(CF_STATS, &[T_CARD])
            .iter()
            .map(|k| de(&k[1..5]))
            .collect()
    }

    // ---- mutation core ----------------------------------------------------

    fn contains_ids(&self, t: IdTriple) -> bool {
        self.db
            .get_cf(self.cf(CF_SPO), key3(t.sub, t.pred, t.obj))
            .ok()
            .flatten()
            .is_some()
    }

    fn insert_ids(&mut self, t: IdTriple) -> bool {
        if self.contains_ids(t) {
            return false;
        }
        let IdTriple { sub, pred, obj } = t;
        let mut wb = WriteBatch::default();
        wb.put_cf(self.cf(CF_SPO), key3(sub, pred, obj), b"");
        wb.put_cf(self.cf(CF_POS), key3(pred, obj, sub), b"");
        wb.put_cf(self.cf(CF_OSP), key3(obj, sub, pred), b"");

        // The cached global counters only advance once the batch is durable, so
        // a failed write leaves them (and the store) exactly as they were.
        let triples = self.triple_count + 1;
        let mut preds = self.num_predicates;
        let mut subjects = self.distinct_subjects;
        let mut objects = self.distinct_objects;
        self.set_global(&mut wb, K_TRIPLES, triples);

        // predicate cardinality + distinct-predicate transition
        if self.step(&mut wb, &tag1(T_CARD, pred), true).0 == 0 {
            preds += 1;
            self.set_global(&mut wb, K_PREDS, preds);
        }
        // distinct subjects / objects
        if self.step(&mut wb, &tag1(T_SUB, sub), true).0 == 0 {
            subjects += 1;
            self.set_global(&mut wb, K_SUBJECTS, subjects);
        }
        if self.step(&mut wb, &tag1(T_OBJ, obj), true).0 == 0 {
            objects += 1;
            self.set_global(&mut wb, K_OBJECTS, objects);
        }
        // per-predicate distinct subjects / objects
        if self.step(&mut wb, &tag2(T_PS, pred, sub), true).0 == 0 {
            self.step(&mut wb, &tag1(T_PSUBJ, pred), true);
        }
        if self.step(&mut wb, &tag2(T_PO, pred, obj), true).0 == 0 {
            self.step(&mut wb, &tag1(T_POBJ, pred), true);
        }

        if let Err(e) = self.db.write(wb) {
            self.record_error("insert", e);
            return false;
        }
        self.triple_count = triples;
        self.num_predicates = preds;
        self.distinct_subjects = subjects;
        self.distinct_objects = objects;
        true
    }

    fn remove_ids(&mut self, t: IdTriple) -> bool {
        if !self.contains_ids(t) {
            return false;
        }
        let IdTriple { sub, pred, obj } = t;
        let mut wb = WriteBatch::default();
        wb.delete_cf(self.cf(CF_SPO), key3(sub, pred, obj));
        wb.delete_cf(self.cf(CF_POS), key3(pred, obj, sub));
        wb.delete_cf(self.cf(CF_OSP), key3(obj, sub, pred));

        let triples = self.triple_count.saturating_sub(1);
        let mut preds = self.num_predicates;
        let mut subjects = self.distinct_subjects;
        let mut objects = self.distinct_objects;
        self.set_global(&mut wb, K_TRIPLES, triples);

        if self.step(&mut wb, &tag1(T_CARD, pred), false).1 == 0 {
            preds = preds.saturating_sub(1);
            self.set_global(&mut wb, K_PREDS, preds);
        }
        if self.step(&mut wb, &tag1(T_SUB, sub), false).1 == 0 {
            subjects = subjects.saturating_sub(1);
            self.set_global(&mut wb, K_SUBJECTS, subjects);
        }
        if self.step(&mut wb, &tag1(T_OBJ, obj), false).1 == 0 {
            objects = objects.saturating_sub(1);
            self.set_global(&mut wb, K_OBJECTS, objects);
        }
        if self.step(&mut wb, &tag2(T_PS, pred, sub), false).1 == 0 {
            self.step(&mut wb, &tag1(T_PSUBJ, pred), false);
        }
        if self.step(&mut wb, &tag2(T_PO, pred, obj), false).1 == 0 {
            self.step(&mut wb, &tag1(T_POBJ, pred), false);
        }

        if let Err(e) = self.db.write(wb) {
            self.record_error("remove", e);
            return false;
        }
        self.triple_count = triples;
        self.num_predicates = preds;
        self.distinct_subjects = subjects;
        self.distinct_objects = objects;
        true
    }
}

// ---- TripleSource (read face) --------------------------------------------

impl TripleSource for RocksStore {
    fn exists(&self, sub: EntityLiteralId, pred: PredId, obj: EntityLiteralId) -> bool {
        self.contains_ids(IdTriple::new(sub, pred, obj))
    }

    /// `s ? ?` → `(pred, obj)` pairs, sorted by `(pred, obj)` (SPO key order).
    fn po_by_s(&self, sub: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        self.scan_keys(CF_SPO, &be(sub))
            .iter()
            .map(|k| (de(&k[4..8]), de(&k[8..12])))
            .collect()
    }

    /// `s p ?` → objects, ascending (SPO key order).
    fn o_by_sp(&self, sub: EntityLiteralId, pred: PredId) -> Vec<EntityLiteralId> {
        self.scan_keys(CF_SPO, &key2(sub, pred))
            .iter()
            .map(|k| de(&k[8..12]))
            .collect()
    }

    /// `s ? o` → predicates linking subject to object, ascending. OSP key is
    /// `o ‖ s ‖ p`, so the `(o, s)` prefix yields predicates in ascending order.
    fn p_by_so(&self, sub: EntityLiteralId, obj: EntityLiteralId) -> Vec<PredId> {
        self.scan_keys(CF_OSP, &key2(obj, sub))
            .iter()
            .map(|k| de(&k[8..12]))
            .collect()
    }

    /// `? ? o` → `(pred, sub)` pairs, sorted by `(pred, sub)` to match
    /// [`TripleStore`](crate::store::TripleStore) (OSP scans in `(s, p)` order).
    fn ps_by_o(&self, obj: EntityLiteralId) -> Vec<(PredId, EntityLiteralId)> {
        let mut out: Vec<(PredId, EntityLiteralId)> = self
            .scan_keys(CF_OSP, &be(obj))
            .iter()
            .map(|k| (de(&k[8..12]), de(&k[4..8])))
            .collect();
        out.sort_unstable();
        out
    }

    /// `? p o` → subjects, ascending (POS `(p, o)` prefix).
    fn s_by_po(&self, pred: PredId, obj: EntityLiteralId) -> Vec<EntityLiteralId> {
        self.scan_keys(CF_POS, &key2(pred, obj))
            .iter()
            .map(|k| de(&k[8..12]))
            .collect()
    }

    /// `? p ?` → `(sub, obj)` pairs, sorted by `(sub, obj)` to match
    /// [`TripleStore`](crate::store::TripleStore) (POS scans in `(o, s)` order).
    fn so_by_p(&self, pred: PredId) -> Vec<(EntityLiteralId, EntityLiteralId)> {
        let mut out: Vec<(EntityLiteralId, EntityLiteralId)> = self
            .scan_keys(CF_POS, &be(pred))
            .iter()
            .map(|k| (de(&k[8..12]), de(&k[4..8])))
            .collect();
        out.sort_unstable();
        out
    }

    fn subs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        let mut v: Vec<EntityLiteralId> = self.s_by_po_all(pred);
        v.sort_unstable();
        v.dedup();
        v
    }

    fn objs_by_p(&self, pred: PredId) -> Vec<EntityLiteralId> {
        let mut v: Vec<EntityLiteralId> = self
            .scan_keys(CF_POS, &be(pred))
            .iter()
            .map(|k| de(&k[4..8]))
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    fn subject_keys(&self) -> Vec<EntityLiteralId> {
        let mut v = self.ids_under(T_SUB);
        v.sort_unstable();
        v
    }

    fn object_keys(&self) -> Vec<EntityLiteralId> {
        let mut v = self.ids_under(T_OBJ);
        v.sort_unstable();
        v
    }

    fn triple_count(&self) -> u64 {
        self.triple_count
    }

    fn distinct_subjects(&self) -> usize {
        self.distinct_subjects as usize
    }

    fn distinct_objects(&self) -> usize {
        self.distinct_objects as usize
    }

    fn num_predicates(&self) -> usize {
        self.num_predicates as usize
    }

    fn pred_card(&self, pred: PredId) -> usize {
        self.get_u64(&tag1(T_CARD, pred)) as usize
    }

    fn pred_distinct_subj(&self, pred: PredId) -> usize {
        self.get_u64(&tag1(T_PSUBJ, pred)) as usize
    }

    fn pred_distinct_obj(&self, pred: PredId) -> usize {
        self.get_u64(&tag1(T_POBJ, pred)) as usize
    }

    fn iter_all(&self) -> Vec<IdTriple> {
        self.scan_keys(CF_SPO, &[])
            .iter()
            .map(|k| IdTriple::new(de(&k[0..4]), de(&k[4..8]), de(&k[8..12])))
            .collect()
    }
}

impl RocksStore {
    /// Subjects (with repetition) under a predicate — the POS `(p, *)` scan
    /// mapped to its subject component. Used by [`subs_by_p`](TripleSource::subs_by_p).
    fn s_by_po_all(&self, pred: PredId) -> Vec<EntityLiteralId> {
        self.scan_keys(CF_POS, &be(pred))
            .iter()
            .map(|k| de(&k[8..12]))
            .collect()
    }
}

// ---- MutableStore (write face) -------------------------------------------

impl MutableStore for RocksStore {
    fn insert(&mut self, t: IdTriple) -> bool {
        self.insert_ids(t)
    }
    fn remove(&mut self, t: IdTriple) -> bool {
        self.remove_ids(t)
    }
    fn bulk_load(&mut self, triples: Vec<IdTriple>) {
        for t in triples {
            self.insert_ids(t);
        }
    }
}

// ---- key encoding ---------------------------------------------------------

/// Options for the triple column families: byte-order = numeric order is given
/// by big-endian keys, so we just enable a space-saving compressor.
fn triple_cf_opts() -> Options {
    let mut o = Options::default();
    o.set_compression_type(DBCompressionType::Lz4);
    o
}

/// 4-byte big-endian id (ordering-preserving).
#[inline]
fn be(x: u32) -> [u8; 4] {
    x.to_be_bytes()
}

/// Decode a 4-byte big-endian id.
#[inline]
fn de(b: &[u8]) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[0..4]);
    u32::from_be_bytes(a)
}

/// 12-byte composite key from three ids.
#[inline]
fn key3(a: u32, b: u32, c: u32) -> [u8; 12] {
    let mut k = [0u8; 12];
    k[0..4].copy_from_slice(&be(a));
    k[4..8].copy_from_slice(&be(b));
    k[8..12].copy_from_slice(&be(c));
    k
}

/// 16-byte quad key `g ‖ s ‖ p ‖ o`.
#[inline]
fn key4(g: u32, s: u32, p: u32, o: u32) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[0..4].copy_from_slice(&be(g));
    k[4..16].copy_from_slice(&key3(s, p, o));
    k
}

/// 8-byte two-id prefix.
#[inline]
fn key2(a: u32, b: u32) -> [u8; 8] {
    let mut k = [0u8; 8];
    k[0..4].copy_from_slice(&be(a));
    k[4..8].copy_from_slice(&be(b));
    k
}

/// Tagged 5-byte counter key: `tag ‖ be(id)`.
#[inline]
fn tag1(tag: u8, id: u32) -> [u8; 5] {
    let mut k = [0u8; 5];
    k[0] = tag;
    k[1..5].copy_from_slice(&be(id));
    k
}

/// Tagged 9-byte counter key: `tag ‖ be(a) ‖ be(b)`.
#[inline]
fn tag2(tag: u8, a: u32, b: u32) -> [u8; 9] {
    let mut k = [0u8; 9];
    k[0] = tag;
    k[1..5].copy_from_slice(&be(a));
    k[5..9].copy_from_slice(&be(b));
    k
}
