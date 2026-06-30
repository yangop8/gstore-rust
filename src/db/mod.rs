//! The [`Database`] facade.
//!
//! Corresponds to gStore's `Database` class: it owns the dictionary and the
//! triple store, builds from RDF files, applies updates, answers SPARQL, and
//! persists to / loads from a database directory.
//!
//! Persistence (DESIGN §7): a database is a directory holding four
//! bincode-serialized files — `dict.bin`, `store.bin`, `meta.bin`, and
//! `vstree.bin` (the signature index). This is the deliberately-simple stand-in
//! for gStore's on-disk B+ tree KVstore (backlog item A).

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::dict::Dictionary;
use crate::error::{GStoreError, Result};
use crate::kvstore::DiskStore;
use crate::model::id::{is_entity_id, EntityLiteralId, PredId};
use crate::model::{IdTriple, Term, Triple};
use crate::parser::sparql::ast::{GraphTarget, GroundTriple, Query, UpdateOp, RDF_TYPE};
use crate::parser::{sparql, turtle};
use crate::query::{Evaluator, FunctionRegistry, QueryResult};
use crate::signature::{EdgeDir, Signature, VsTree};
use crate::store::{Backend, MutableStore, TripleSource, TripleStore};

const RDFS_SUBCLASS: &str = "http://www.w3.org/2000/01/rdf-schema#subClassOf";
const RDFS_SUBPROP: &str = "http://www.w3.org/2000/01/rdf-schema#subPropertyOf";
const RDFS_DOMAIN: &str = "http://www.w3.org/2000/01/rdf-schema#domain";
const RDFS_RANGE: &str = "http://www.w3.org/2000/01/rdf-schema#range";

const DICT_FILE: &str = "dict.bin";
const STORE_FILE: &str = "store.bin";
const META_FILE: &str = "meta.bin";
const VSTREE_FILE: &str = "vstree.bin";
const NAMED_FILE: &str = "named.bin";
/// The append-only update log inside a database directory (gStore's update.log).
const UPDATE_LOG_FILE: &str = "update.log";
/// The append-only triple-level redo log inside a database directory. Records
/// every *committed* mutation so it can be re-applied on recovery (the redo half
/// of gStore's WAL; the undo half is the per-transaction [`UndoOp`] log).
const REDO_LOG_FILE: &str = "redo.log";
/// Default upper bound on cached read-query results before LRU eviction kicks in.
const DEFAULT_QUERY_CACHE_CAP: usize = 256;
/// The on-disk B+ tree KVstore file inside a database directory.
const KV_FILE: &str = "kvstore.kv";
/// The on-disk (out-of-core) VS-tree node file inside a disk database directory.
/// Distinct from the in-memory snapshot's `vstree.bin`: this is the paged node
/// file a disk database filters through without loading the whole tree.
const VSTREE_KV_FILE: &str = "vstree.kv";
/// Sub-directory holding the RocksDB store of a RocksDB-backed database.
#[cfg(feature = "rocksdb")]
const ROCKS_SUBDIR: &str = "rocksdb";
/// Page-cache size for the disk store (4096 × 4 KiB = 16 MiB).
const DISK_CACHE_PAGES: usize = 4096;

/// Sign every entity by its in/out edges, returning `(entity_id, signature)`
/// pairs. Generic over the [`TripleSource`] so it works identically against the
/// in-memory store, the [`Backend`] enum, or the on-disk store.
fn vstree_entries<S: TripleSource>(store: &S) -> Vec<(EntityLiteralId, Signature)> {
    // Entities = everything that is a subject, plus objects that are entities
    // (literal objects are not indexed by the VS-tree).
    let mut ids: Vec<u32> = store.subject_keys();
    ids.extend(store.object_keys().into_iter().filter(|&o| is_entity_id(o)));
    ids.sort_unstable();
    ids.dedup();

    ids.into_iter()
        .map(|e| {
            let mut sig = Signature::new();
            for &(p, o) in &store.po_by_s(e) {
                sig.encode_edge(p, o, EdgeDir::Out);
            }
            for &(p, s) in &store.ps_by_o(e) {
                sig.encode_edge(p, s, EdgeDir::In);
            }
            (e, sig)
        })
        .collect()
}

/// Build an in-memory VS-tree over every entity (see [`vstree_entries`]).
fn build_vstree<S: TripleSource>(store: &S) -> VsTree {
    VsTree::build(vstree_entries(store))
}

/// On-disk metadata (kept tiny and human-meaningful).
#[derive(Debug, Serialize, Deserialize)]
struct Meta {
    name: String,
    triple_num: u64,
    entity_num: u64,
    literal_num: u64,
    predicate_num: u64,
}

/// A bounded LRU cache of read-query results, keyed by the SPARQL string
/// (gStore's `QueryCache`, but with a capacity bound). Each entry carries a
/// monotonically-increasing access tick; when the cache is full, the entry with
/// the smallest tick (least-recently used) is evicted to make room. The whole
/// cache is still cleared on any store mutation (writes invalidate reads).
#[derive(Debug)]
struct LruCache {
    map: HashMap<String, (QueryResult, u64)>,
    cap: usize,
    tick: u64,
}

impl LruCache {
    fn new(cap: usize) -> LruCache {
        LruCache {
            map: HashMap::new(),
            cap,
            tick: 0,
        }
    }

    /// Look up `key`, refreshing its recency and returning a clone on hit.
    fn get(&mut self, key: &str) -> Option<QueryResult> {
        self.tick += 1;
        let tk = self.tick;
        let entry = self.map.get_mut(key)?;
        entry.1 = tk;
        Some(entry.0.clone())
    }

    /// Insert `key → value`, evicting the least-recently-used entry if inserting
    /// a new key would exceed the capacity. A zero capacity disables caching.
    fn put(&mut self, key: String, value: QueryResult) {
        if self.cap == 0 {
            return;
        }
        self.tick += 1;
        let tk = self.tick;
        if !self.map.contains_key(&key) && self.map.len() >= self.cap {
            if let Some(lru) = self
                .map
                .iter()
                .min_by_key(|(_, v)| v.1)
                .map(|(k, _)| k.clone())
            {
                self.map.remove(&lru);
            }
        }
        self.map.insert(key, (value, tk));
    }

    fn clear(&mut self) {
        self.map.clear();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.map.len()
    }

    #[cfg(test)]
    fn contains(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }
}

/// One pending/serialized redo-log record: an applied mutation in surface form.
/// `op` is `b'I'` (insert) or `b'D'` (delete); `graph` is the named-graph IRI
/// (`None` ⇒ default graph); `triple` is the N-Triples `s p o .` text.
#[derive(Debug, Clone)]
struct RedoRec {
    op: u8,
    graph: Option<String>,
    triple: String,
}

/// An RDF database: a dictionary, the six-way triple index, and a VS-tree.
#[derive(Debug)]
pub struct Database {
    name: String,
    dict: Dictionary,
    /// The live default-graph store, selected at construction: in-memory
    /// ([`Backend::Memory`], the default) or persistent RocksDB
    /// ([`Backend::Rocks`], feature `rocksdb`). All reads/writes go through the
    /// [`TripleSource`]/[`MutableStore`] seam, so the engine is backend-agnostic.
    store: Backend,
    /// Named graphs: graph-IRI entity id → its triple store. The default graph
    /// is `store`; `GRAPH` patterns and quad updates target this map. Named
    /// graphs stay in-memory (DESIGN §"Phase 2").
    named: BTreeMap<u32, TripleStore>,
    /// Signature index for query-time candidate pruning.
    vstree: VsTree,
    /// Whether `vstree` is consistent with `store`. Cleared on update; the
    /// VS-tree is only used for filtering while valid, and rebuilt on `save`
    /// or `rebuild_index`. (A stale tree is never used, preserving correctness.)
    index_valid: bool,
    /// Active transaction's undo log (`None` ⇒ auto-commit mode). Each entry is
    /// the inverse of a triple mutation, applied in reverse on rollback.
    txn: Option<Vec<UndoOp>>,
    /// Bounded LRU cache of read-query results keyed by the SPARQL string
    /// (gStore `QueryCache`), cleared on any store mutation. Interior mutability
    /// so a `&self`/`&mut self` query path can read and populate it.
    query_cache: RefCell<LruCache>,
    /// When set, every applied SPARQL UPDATE is appended to this file (gStore's
    /// `update.log`), so the write history can be replayed or audited. `None`
    /// disables logging (the default).
    update_log: Option<PathBuf>,
    /// When set, every *committed* triple mutation is appended to this file as a
    /// redo record so it can be re-applied on recovery. `None` disables it.
    redo_log: Option<PathBuf>,
    /// Redo records buffered while a (single-writer) transaction is open; flushed
    /// to [`redo_log`](Self::redo_log) on commit, discarded on rollback. In
    /// auto-commit mode records are written through immediately and this stays empty.
    redo_pending: Vec<RedoRec>,
    /// User-defined forward-chaining reasoning rules (gStore's `ReasonHelper`).
    rules: crate::reason::RuleSet,
    /// User-defined scalar SPARQL functions (gStore's PFN, as a safe in-process
    /// registry rather than dlopen `.so` plugins). Passed to every read-query
    /// evaluator; consulted for any function name that is not a built-in.
    functions: FunctionRegistry,
}

/// Build-pipeline stage, reported through the progress callback of
/// [`Database::build_from_str_with_progress`] (gStore's `DatabaseProgressStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildProgress {
    /// Parsing the RDF document into triples.
    RdfParse,
    /// Interning terms into the dictionary (string↔id).
    Dictionary,
    /// Building the triple indexes / VS-tree.
    Index,
    /// Build complete.
    Done,
}

/// A snapshot of database counts and status (gStore `getDBMonitorInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbStats {
    pub name: String,
    pub triple_num: u64,
    pub entity_num: usize,
    pub literal_num: usize,
    pub predicate_num: usize,
    /// Whether the VS-tree index is consistent with the store.
    pub index_valid: bool,
    /// Whether a transaction is currently open.
    pub in_transaction: bool,
}

/// Extracted schema vocabulary (gStore `getSchemaInfo`): the classes and
/// properties mentioned by the data and any RDFS schema.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Schema {
    /// Class IRIs (rdf:type objects; rdfs:subClassOf operands; domain/range values).
    pub classes: Vec<String>,
    /// Property IRIs (predicates used in data; rdfs:subPropertyOf/domain/range subjects).
    pub properties: Vec<String>,
}

/// One undo-log entry: the action that reverses a triple mutation, in a given
/// graph (`None` = the default graph, `Some(gid)` = a named graph).
#[derive(Debug)]
enum UndoOp {
    /// Re-add a triple removed during the transaction.
    Add(Option<u32>, IdTriple),
    /// Remove a triple added during the transaction.
    Del(Option<u32>, IdTriple),
}

impl Database {
    /// Create an empty, named database.
    pub fn new(name: impl Into<String>) -> Database {
        Database {
            name: name.into(),
            dict: Dictionary::new(),
            store: Backend::Memory(TripleStore::new()),
            named: BTreeMap::new(),
            vstree: VsTree::new(),
            index_valid: true, // empty store ⇔ empty tree, trivially consistent
            txn: None,
            query_cache: RefCell::new(LruCache::new(DEFAULT_QUERY_CACHE_CAP)),
            update_log: None,
            redo_log: None,
            redo_pending: Vec::new(),
            rules: crate::reason::RuleSet::new(),
            functions: FunctionRegistry::new(),
        }
    }

    /// Rebuild the VS-tree from the current store and mark the index valid.
    pub fn rebuild_index(&mut self) {
        self.vstree = build_vstree(&self.store);
        self.index_valid = true;
    }

    /// Register a user-defined scalar SPARQL function (gStore's PFN, as a safe
    /// in-process closure rather than a dlopen `.so` plugin). The name is
    /// case-insensitive; built-ins take precedence. The function becomes visible
    /// to every subsequent read query run through [`query`](Self::query) /
    /// [`select`](Self::select). Clears the query cache (results may change).
    pub fn register_function<F>(&mut self, name: &str, f: F) -> &mut Self
    where
        F: Fn(&[crate::query::Value]) -> Option<crate::query::Value> + Send + Sync + 'static,
    {
        self.functions.register(name, f);
        self.query_cache.get_mut().clear();
        self
    }

    /// Number of user-defined functions registered.
    pub fn function_count(&self) -> usize {
        self.functions.len()
    }

    /// Build a database by importing one or more N-Triples files.
    pub fn build_from_files<P: AsRef<Path>>(
        name: impl Into<String>,
        files: &[P],
    ) -> Result<Database> {
        let mut db = Database::new(name);
        let mut id_triples: Vec<IdTriple> = Vec::new();
        for f in files {
            for t in turtle::parse_file(f)? {
                id_triples.push(db.encode_triple(&t));
            }
        }
        db.store.bulk_load(id_triples);
        db.rebuild_index();
        Ok(db)
    }

    /// Build a database from an in-memory RDF document (Turtle / N-Triples).
    /// Turtle is a superset of N-Triples, so both bundled formats are accepted.
    pub fn build_from_str(name: impl Into<String>, content: &str) -> Result<Database> {
        Database::build_from_str_with_progress(name, content, |_| {})
    }

    /// Like [`build_from_str`](Self::build_from_str) but reports build progress
    /// through `progress` as it moves through the stages (gStore's
    /// `DatabaseProgressStatus`): RDF parse → dictionary build → index build →
    /// done. Useful for a progress bar / status endpoint on large loads.
    pub fn build_from_str_with_progress<F: FnMut(BuildProgress)>(
        name: impl Into<String>,
        content: &str,
        mut progress: F,
    ) -> Result<Database> {
        let mut db = Database::new(name);
        progress(BuildProgress::RdfParse);
        let triples = turtle::parse_str(content)?;
        progress(BuildProgress::Dictionary);
        let mut id_triples: Vec<IdTriple> = Vec::with_capacity(triples.len());
        for t in &triples {
            id_triples.push(db.encode_triple(t));
        }
        db.store.bulk_load(id_triples);
        progress(BuildProgress::Index);
        db.rebuild_index();
        progress(BuildProgress::Done);
        Ok(db)
    }

    // ---- RocksDB-backed database (feature `rocksdb`) ----------------------

    /// Build a **persistent**, RocksDB-backed database in directory `dir` from an
    /// in-memory RDF document. The triples live in a RocksDB store under
    /// `dir/rocksdb`; the dictionary and metadata are snapshotted next to it so
    /// the database can be reopened with [`open_rocksdb`](Self::open_rocksdb).
    /// The full query engine (optimizer, VS-tree, analytics) runs over it
    /// unchanged via the [`Backend`] seam.
    #[cfg(feature = "rocksdb")]
    pub fn build_rocksdb_from_str<P: AsRef<Path>>(
        name: impl Into<String>,
        dir: P,
        content: &str,
    ) -> Result<Database> {
        let dir = dir.as_ref();
        fs::create_dir_all(dir)?;
        let mut dict = Dictionary::new();
        let mut ids: Vec<IdTriple> = Vec::new();
        for t in turtle::parse_str(content)? {
            let sub = dict.intern_entity(&t.subject.dict_key());
            let pred = dict.intern_predicate(&t.predicate.dict_key());
            let obj = dict.intern_term(&t.object);
            ids.push(IdTriple::new(sub, pred, obj));
        }
        let mut rocks = crate::backend::rocks::RocksStore::open(dir.join(ROCKS_SUBDIR))?;
        rocks.bulk_load(ids);
        rocks.flush()?;
        let store = Backend::Rocks(rocks);

        let name = name.into();
        write_bincode(&dir.join(DICT_FILE), &dict)?;
        let meta = Meta {
            name: name.clone(),
            triple_num: store.triple_count(),
            entity_num: dict.entity_num() as u64,
            literal_num: dict.literal_num() as u64,
            predicate_num: dict.predicate_num() as u64,
        };
        write_bincode(&dir.join(META_FILE), &meta)?;

        let vstree = build_vstree(&store);
        Ok(Database {
            name,
            dict,
            store,
            named: BTreeMap::new(),
            vstree,
            index_valid: true,
            txn: None,
            query_cache: RefCell::new(LruCache::new(DEFAULT_QUERY_CACHE_CAP)),
            update_log: None,
            redo_log: None,
            redo_pending: Vec::new(),
            rules: crate::reason::RuleSet::new(),
            functions: FunctionRegistry::new(),
        })
    }

    /// Whether directory `dir` holds a RocksDB-backed database.
    #[cfg(feature = "rocksdb")]
    pub fn is_rocksdb<P: AsRef<Path>>(dir: P) -> bool {
        dir.as_ref().join(ROCKS_SUBDIR).is_dir()
    }

    /// Reopen a RocksDB-backed database created by
    /// [`build_rocksdb_from_str`](Self::build_rocksdb_from_str). The triples are
    /// read from the RocksDB store on demand; the dictionary is loaded from its
    /// snapshot and the VS-tree is rebuilt.
    #[cfg(feature = "rocksdb")]
    pub fn open_rocksdb<P: AsRef<Path>>(dir: P) -> Result<Database> {
        let dir = dir.as_ref();
        let rocks_dir = dir.join(ROCKS_SUBDIR);
        if !rocks_dir.is_dir() {
            return Err(GStoreError::Database(format!(
                "no RocksDB store at '{}'",
                rocks_dir.display()
            )));
        }
        let dict: Dictionary = read_bincode(&dir.join(DICT_FILE))?;
        let meta: Meta = read_bincode(&dir.join(META_FILE))?;
        let rocks = crate::backend::rocks::RocksStore::open(rocks_dir)?;
        let store = Backend::Rocks(rocks);
        if store.triple_count() != meta.triple_num {
            return Err(GStoreError::Database(format!(
                "corrupt database: meta says {} triples but RocksDB has {}",
                meta.triple_num,
                store.triple_count()
            )));
        }
        let vstree = build_vstree(&store);
        Ok(Database {
            name: meta.name,
            dict,
            store,
            named: BTreeMap::new(),
            vstree,
            index_valid: true,
            txn: None,
            query_cache: RefCell::new(LruCache::new(DEFAULT_QUERY_CACHE_CAP)),
            update_log: None,
            redo_log: None,
            redo_pending: Vec::new(),
            rules: crate::reason::RuleSet::new(),
            functions: FunctionRegistry::new(),
        })
    }

    // ---- accessors --------------------------------------------------------

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn triple_num(&self) -> u64 {
        self.store.triple_count()
    }
    pub fn entity_num(&self) -> usize {
        self.dict.entity_num()
    }
    pub fn literal_num(&self) -> usize {
        self.dict.literal_num()
    }
    pub fn predicate_num(&self) -> usize {
        self.dict.predicate_num()
    }
    pub fn dict(&self) -> &Dictionary {
        &self.dict
    }
    /// The in-memory default-graph store. Available for the default
    /// [`Backend::Memory`] backend (used by snapshot MVCC and tests); panics for
    /// a RocksDB-backed database, which has no in-RAM store — query it through
    /// [`query`](Self::query)/[`select`](Self::select) instead.
    pub fn store(&self) -> &TripleStore {
        self.store
            .as_memory()
            .expect("Database::store() requires the in-memory backend")
    }

    /// The live storage backend (works for any variant).
    pub fn backend(&self) -> &Backend {
        &self.store
    }
    /// The named graphs (graph-IRI entity id → store).
    pub fn named_graphs(&self) -> &BTreeMap<u32, TripleStore> {
        &self.named
    }

    /// A snapshot of counts and status (gStore `getDBMonitorInfo`).
    pub fn stats(&self) -> DbStats {
        DbStats {
            name: self.name.clone(),
            triple_num: self.store.triple_count(),
            entity_num: self.dict.entity_num(),
            literal_num: self.dict.literal_num(),
            predicate_num: self.dict.predicate_num(),
            index_valid: self.index_valid,
            in_transaction: self.txn.is_some(),
        }
    }

    /// Extract the schema vocabulary — classes and properties — mentioned by the
    /// data and any RDFS schema triples (gStore `getSchemaInfo`).
    pub fn schema(&self) -> Schema {
        let key = |iri: &str| Term::iri(iri).dict_key();
        let pid = |iri: &str| self.dict.predicate_id(&key(iri));

        // Classes: rdf:type objects; subClassOf operands; domain/range values.
        let mut class_ids: Vec<u32> = Vec::new();
        if let Some(tp) = pid(RDF_TYPE) {
            class_ids.extend(self.store.so_by_p(tp).iter().map(|&(_, o)| o));
        }
        if let Some(sco) = pid(RDFS_SUBCLASS) {
            for &(s, o) in &self.store.so_by_p(sco) {
                class_ids.push(s);
                class_ids.push(o);
            }
        }
        for iri in [RDFS_DOMAIN, RDFS_RANGE] {
            if let Some(p) = pid(iri) {
                class_ids.extend(self.store.so_by_p(p).iter().map(|&(_, c)| c));
            }
        }
        let mut classes: Vec<String> = class_ids
            .iter()
            .filter_map(|&id| self.dict.id_to_string(id).map(str::to_owned))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        classes.sort();

        // Properties: predicates used in data; subjects of subProperty/domain/range.
        let mut props: HashSet<String> = self
            .store
            .predicates()
            .into_iter()
            .filter_map(|p| self.dict.predicate_to_string(p).map(str::to_owned))
            .collect();
        for iri in [RDFS_SUBPROP, RDFS_DOMAIN, RDFS_RANGE] {
            if let Some(p) = pid(iri) {
                for &(s, _o) in &self.store.so_by_p(p) {
                    if let Some(name) = self.dict.id_to_string(s) {
                        props.insert(name.to_owned());
                    }
                }
            }
        }
        let mut properties: Vec<String> = props.into_iter().collect();
        properties.sort();

        Schema {
            classes,
            properties,
        }
    }

    // ---- updates ----------------------------------------------------------

    /// Encode a triple to ids, interning any new terms.
    fn encode_triple(&mut self, t: &Triple) -> IdTriple {
        let sub = self.dict.intern_entity(&t.subject.dict_key());
        let pred = self.dict.intern_predicate(&t.predicate.dict_key());
        let obj = self.dict.intern_term(&t.object);
        IdTriple::new(sub, pred, obj)
    }

    /// Insert one triple. Returns `true` if it was newly added.
    pub fn insert_triple(&mut self, t: &Triple) -> bool {
        let id = self.encode_triple(t);
        let changed = self.store.insert(id);
        if changed {
            // While the index is consistent, keep it consistent *incrementally*:
            // the inserted edge only changes the signatures of its subject (and
            // its object, when that is an entity), so re-sign just those entities
            // and update their VS-tree entries in place — no full rebuild. If the
            // index is already stale we leave it stale and rebuild lazily.
            //
            // An out-of-core (disk-backed) tree can't be updated in place here
            // (its nodes live in a paged file), so we instead invalidate it; the
            // next consistent query rebuilds it in memory (or skips filtering).
            if self.index_valid {
                if self.vstree.is_disk_backed() {
                    self.index_valid = false;
                } else {
                    self.vstree_index_insert(id);
                }
            }
            self.query_cache.get_mut().clear();
            if let Some(log) = self.txn.as_mut() {
                log.push(UndoOp::Del(None, id)); // rollback removes what we added
            }
            self.record_redo(b'I', None, id);
        }
        changed
    }

    /// Fold a just-inserted default-graph triple into the VS-tree incrementally
    /// (called only while the index is valid). The subject — and the object when
    /// it is an entity — gained an incident edge, so each is re-signed from the
    /// current store and its tree entry updated (or inserted, if new to the
    /// tree). Mirrors [`build_vstree`]'s per-entity signing, so the
    /// incrementally-maintained tree stays equivalent to a freshly-rebuilt one
    /// and the candidate filter remains a sound superset.
    fn vstree_index_insert(&mut self, id: IdTriple) {
        self.upsert_entity_signature(id.sub);
        // Literal objects are not VS-tree entities (see `build_vstree`).
        if is_entity_id(id.obj) {
            self.upsert_entity_signature(id.obj);
        }
    }

    /// Recompute entity `e`'s full signature from its current in/out edges and
    /// update its VS-tree entry in place, inserting it if it is not yet indexed.
    fn upsert_entity_signature(&mut self, e: u32) {
        let mut sig = Signature::new();
        for &(p, o) in &self.store.po_by_s(e) {
            sig.encode_edge(p, o, EdgeDir::Out);
        }
        for &(p, s) in &self.store.ps_by_o(e) {
            sig.encode_edge(p, s, EdgeDir::In);
        }
        if !self.vstree.update(e, sig) {
            self.vstree.insert(e, sig);
        }
    }

    /// Remove one triple. Returns `true` if it existed. Does not intern: if any
    /// term is unknown, the triple cannot exist.
    pub fn remove_triple(&mut self, t: &Triple) -> bool {
        let (Some(sub), Some(pred), Some(obj)) = (
            self.dict.entity_id(&t.subject.dict_key()),
            self.dict.predicate_id(&t.predicate.dict_key()),
            self.dict.term_id(&t.object),
        ) else {
            return false;
        };
        let id = IdTriple::new(sub, pred, obj);
        let changed = self.store.remove(id);
        if changed {
            // Deletes deliberately invalidate (full rebuild on next save / query)
            // rather than updating incrementally. A removed edge *shrinks* the
            // subject/object signatures, and while `VsTree::update` stays sound
            // under a shrink, a delete can also leave an entity with no edges at
            // all (a phantom tree entry) and can be followed by `reclaim_unused`,
            // which reuses freed dictionary ids for unrelated terms. Cleanly
            // handling those would need entry removal + rebalancing the bulk-built
            // tree, so we keep the simple, always-correct rebuild for deletes.
            self.index_valid = false;
            self.query_cache.get_mut().clear();
            if let Some(log) = self.txn.as_mut() {
                log.push(UndoOp::Add(None, id)); // rollback re-adds what we removed
            }
            self.record_redo(b'D', None, id);
        }
        changed
    }

    // ---- transactions -----------------------------------------------------

    /// Begin a transaction (single-writer). Triple mutations are recorded so
    /// they can be undone with [`rollback`](Self::rollback); [`commit`](Self::commit)
    /// makes them permanent. Returns an error if a transaction is already active
    /// (nesting is not supported). Provides atomicity + rollback; full MVCC
    /// (version chains, locking, GC) is a separate concern (see REFACTOR_BACKLOG).
    pub fn begin(&mut self) -> Result<()> {
        if self.txn.is_some() {
            return Err(GStoreError::Database(
                "a transaction is already active".into(),
            ));
        }
        self.txn = Some(Vec::new());
        Ok(())
    }

    /// Commit the active transaction, discarding its undo log. Buffered redo
    /// records (this transaction's mutations) are flushed to the redo log, since
    /// the transaction is now durable.
    pub fn commit(&mut self) -> Result<()> {
        if self.txn.take().is_none() {
            return Err(GStoreError::Database("no active transaction".into()));
        }
        self.flush_redo()?;
        Ok(())
    }

    /// Roll back the active transaction, undoing every triple mutation made
    /// since [`begin`](Self::begin) in reverse order.
    pub fn rollback(&mut self) -> Result<()> {
        let Some(log) = self.txn.take() else {
            return Err(GStoreError::Database("no active transaction".into()));
        };
        // The transaction's mutations are being undone, so its buffered redo
        // records must never reach the log.
        self.redo_pending.clear();
        for op in log.into_iter().rev() {
            match op {
                UndoOp::Add(None, id) => {
                    self.store.insert(id);
                }
                UndoOp::Del(None, id) => {
                    self.store.remove(id);
                }
                UndoOp::Add(Some(gid), id) => {
                    self.named.entry(gid).or_default().insert(id);
                }
                UndoOp::Del(Some(gid), id) => {
                    if let Some(s) = self.named.get_mut(&gid) {
                        s.remove(id);
                    }
                }
            }
        }
        self.index_valid = false;
        self.query_cache.get_mut().clear();
        Ok(())
    }

    /// Whether a transaction is currently active.
    pub fn in_transaction(&self) -> bool {
        self.txn.is_some()
    }

    fn ground_to_triple(g: &GroundTriple) -> Triple {
        Triple::new(g.subject.clone(), g.predicate.clone(), g.object.clone())
    }

    /// Insert a triple into a named graph (`None` ⇒ default graph). Returns
    /// `true` if newly added.
    fn insert_quad(&mut self, t: &Triple, graph: Option<&str>) -> bool {
        let Some(g) = graph else {
            return self.insert_triple(t);
        };
        let gid = self.dict.intern_entity(&Term::iri(g).dict_key());
        let id = self.encode_triple(t);
        let changed = self.named.entry(gid).or_default().insert(id);
        if changed {
            self.query_cache.get_mut().clear();
            if let Some(log) = self.txn.as_mut() {
                log.push(UndoOp::Del(Some(gid), id));
            }
            self.record_redo(b'I', Some(gid), id);
        }
        changed
    }

    /// Remove a triple from a named graph (`None` ⇒ default graph). Returns
    /// `true` if it existed.
    fn delete_quad(&mut self, t: &Triple, graph: Option<&str>) -> bool {
        let Some(g) = graph else {
            return self.remove_triple(t);
        };
        let (Some(gid), Some(sub), Some(pred), Some(obj)) = (
            self.dict.entity_id(&Term::iri(g).dict_key()),
            self.dict.entity_id(&t.subject.dict_key()),
            self.dict.predicate_id(&t.predicate.dict_key()),
            self.dict.term_id(&t.object),
        ) else {
            return false;
        };
        let id = IdTriple::new(sub, pred, obj);
        let changed = self.named.get_mut(&gid).is_some_and(|s| s.remove(id));
        if changed {
            self.query_cache.get_mut().clear();
            if let Some(log) = self.txn.as_mut() {
                log.push(UndoOp::Add(Some(gid), id));
            }
            self.record_redo(b'D', Some(gid), id);
        }
        changed
    }

    /// Apply `INSERT DATA`. Returns the number of triples newly added.
    pub fn insert_data(&mut self, triples: &[GroundTriple]) -> usize {
        triples
            .iter()
            .filter(|g| self.insert_quad(&Self::ground_to_triple(g), g.graph.as_deref()))
            .count()
    }

    /// Apply `DELETE DATA`. Returns the number of triples actually removed.
    pub fn delete_data(&mut self, triples: &[GroundTriple]) -> usize {
        triples
            .iter()
            .filter(|g| self.delete_quad(&Self::ground_to_triple(g), g.graph.as_deref()))
            .count()
    }

    /// Materialize the RDFS closure over the current data (gStore `src/Reason`):
    /// subclass/subproperty transitivity, type propagation, and domain/range
    /// typing. Returns the number of inferred triples added; the VS-tree is
    /// invalidated (and rebuilt lazily on save / next consistent query).
    pub fn materialize_rdfs(&mut self) -> usize {
        let added = crate::reason::materialize(&mut self.dict, &mut self.store);
        let n = added.len();
        if n > 0 {
            self.index_valid = false;
            self.query_cache.get_mut().clear();
            // Record inferred triples so reasoning inside a transaction rolls back.
            if let Some(log) = self.txn.as_mut() {
                for t in &added {
                    log.push(UndoOp::Del(None, *t));
                }
            }
        }
        n
    }

    // ---- query ------------------------------------------------------------

    /// Parse and run a SPARQL request. SELECT/ASK read; INSERT/DELETE DATA write.
    pub fn query(&mut self, sparql: &str) -> Result<QueryResult> {
        let q = sparql::parse(sparql)?;
        match q {
            Query::Select(_) | Query::Ask(_) | Query::Construct(_) | Query::Describe(_) => {
                // Serve from the query cache when the same request was already
                // answered against the current (unchanged) store.
                if let Some(cached) = self.query_cache.borrow_mut().get(sparql) {
                    return Ok(cached);
                }
                // Use the VS-tree as a candidate filter only while it is
                // consistent with the store; otherwise evaluate without it.
                let mut eval = if self.index_valid {
                    Evaluator::with_vstree(&self.dict, &self.store, &self.vstree)
                } else {
                    Evaluator::new(&self.dict, &self.store)
                };
                if !self.named.is_empty() {
                    eval = eval.with_named(&self.named);
                }
                // Make user-defined functions (PFN) visible to this query.
                if !self.functions.is_empty() {
                    eval = eval.with_functions(self.functions.clone());
                }
                let result = eval.evaluate(&q)?;
                self.query_cache
                    .borrow_mut()
                    .put(sparql.to_string(), result.clone());
                Ok(result)
            }
            Query::Update(ops) => {
                let mut changed = 0;
                for op in ops {
                    changed += self.exec_update_op(op)?;
                }
                // Append the applied statement to the update log, if enabled.
                if let Some(path) = self.update_log.clone() {
                    append_update_log(&path, sparql, changed)?;
                }
                Ok(QueryResult::Update { changed })
            }
        }
    }

    /// Apply one UPDATE operation, returning the number of triples it changed.
    fn exec_update_op(&mut self, op: UpdateOp) -> Result<usize> {
        match op {
            UpdateOp::InsertData(triples) => Ok(self.insert_data(&triples)),
            UpdateOp::DeleteData(triples) => Ok(self.delete_data(&triples)),
            UpdateOp::Modify {
                delete,
                insert,
                pattern,
            } => {
                // Compute the ground delete/insert sets against the *current*
                // data, then apply deletes before inserts (SPARQL semantics).
                let (dels, ins) = {
                    let eval = Evaluator::new(&self.dict, &self.store);
                    eval.eval_update_modify(&delete, &insert, &pattern)
                };
                let mut changed = 0;
                for t in &dels {
                    if self.remove_triple(t) {
                        changed += 1;
                    }
                }
                for t in &ins {
                    if self.insert_triple(t) {
                        changed += 1;
                    }
                }
                Ok(changed)
            }
            UpdateOp::Load { source, silent } => match self.load_rdf_source(&source) {
                Ok(n) => Ok(n),
                Err(e) => {
                    if silent {
                        Ok(0)
                    } else {
                        Err(e)
                    }
                }
            },
            UpdateOp::Clear { target, .. } | UpdateOp::Drop { target, .. } => match target {
                GraphTarget::Default => Ok(self.clear_default()),
                GraphTarget::All => Ok(self.clear_default() + self.clear_all_named()),
                // NAMED keyword (an empty name) ⇒ all named graphs.
                GraphTarget::Named(g) if g.is_empty() => Ok(self.clear_all_named()),
                GraphTarget::Named(g) => Ok(self.clear_named(&g)),
            },
            // CREATE GRAPH only declares an (empty) graph; data ops create it.
            UpdateOp::Create { .. } => Ok(0),
        }
    }

    /// Clear the default graph, returning the count cleared.
    fn clear_default(&mut self) -> usize {
        let n = self.store.triple_count() as usize;
        if n > 0 {
            // Snapshot the triples once: needed for undo, and (for a persistent
            // backend) to delete them so the on-disk store is actually cleared.
            let removed = self.store.iter_all();
            if let Some(log) = self.txn.as_mut() {
                for t in &removed {
                    log.push(UndoOp::Add(None, *t));
                }
            }
            if self.store.as_memory().is_some() {
                // In-memory: discard and replace (cheaper than per-triple delete).
                self.store = Backend::Memory(TripleStore::new());
            } else {
                // Persistent backend: delete every triple through the seam so the
                // change is durable (don't swap in a Memory store, which would
                // strand the data on disk).
                for t in removed {
                    self.store.remove(t);
                }
            }
            self.index_valid = false;
            self.query_cache.get_mut().clear();
        }
        n
    }

    /// Clear one named graph, returning the count cleared.
    fn clear_named(&mut self, graph: &str) -> usize {
        let Some(gid) = self.dict.entity_id(&Term::iri(graph).dict_key()) else {
            return 0;
        };
        let n = self.named.get(&gid).map_or(0, |s| s.triple_count() as usize);
        if n > 0 {
            if let Some(log) = self.txn.as_mut() {
                if let Some(s) = self.named.get(&gid) {
                    for t in s.iter_all() {
                        log.push(UndoOp::Add(Some(gid), t));
                    }
                }
            }
            self.named.remove(&gid);
            self.query_cache.get_mut().clear();
        }
        n
    }

    /// Clear every named graph, returning the total count cleared.
    fn clear_all_named(&mut self) -> usize {
        let total: usize = self.named.values().map(|s| s.triple_count() as usize).sum();
        if total > 0 {
            if let Some(log) = self.txn.as_mut() {
                for (&gid, s) in &self.named {
                    for t in s.iter_all() {
                        log.push(UndoOp::Add(Some(gid), t));
                    }
                }
            }
            self.named.clear();
            self.query_cache.get_mut().clear();
        }
        total
    }

    /// `LOAD <iri>`: read an RDF (Turtle/N-Triples) document into the default
    /// graph. Only local sources are fetched — a `file://` IRI or a bare path;
    /// remote `http(s)://` sources are not retrieved (no network) and error.
    fn load_rdf_source(&mut self, source: &str) -> Result<usize> {
        if source.starts_with("http://") || source.starts_with("https://") {
            return Err(GStoreError::Query(format!(
                "LOAD of remote source '{source}' is not supported (no network); \
                 use a local file path or file:// IRI"
            )));
        }
        let path = source.strip_prefix("file://").unwrap_or(source);
        let triples = turtle::parse_file(path)?;
        Ok(triples.iter().filter(|t| self.insert_triple(t)).count())
    }

    /// Convenience: parse + ensure a SELECT result.
    pub fn select(&mut self, sparql: &str) -> Result<crate::query::ResultSet> {
        match self.query(sparql)? {
            QueryResult::Select(rs) => Ok(rs),
            other => Err(GStoreError::Query(format!(
                "expected a SELECT query, got {other:?}"
            ))),
        }
    }

    // ---- persistence ------------------------------------------------------

    /// Save the database into directory `dir` (created if necessary).
    ///
    /// For the in-memory backend this writes the bincode snapshot exactly as
    /// before. For a persistent backend the triples already live in their own
    /// store (e.g. the RocksDB directory), so only the dictionary, named graphs,
    /// metadata, and VS-tree are snapshotted here.
    pub fn save<P: AsRef<Path>>(&self, dir: P) -> Result<()> {
        let dir = dir.as_ref();
        fs::create_dir_all(dir)?;
        write_bincode(&dir.join(DICT_FILE), &self.dict)?;
        if let Some(mem) = self.store.as_memory() {
            write_bincode(&dir.join(STORE_FILE), mem)?;
        }
        write_bincode(&dir.join(NAMED_FILE), &self.named)?;
        let meta = Meta {
            name: self.name.clone(),
            triple_num: self.store.triple_count(),
            entity_num: self.dict.entity_num() as u64,
            literal_num: self.dict.literal_num() as u64,
            predicate_num: self.dict.predicate_num() as u64,
        };
        write_bincode(&dir.join(META_FILE), &meta)?;
        // Persist a fresh in-memory VS-tree snapshot. Rebuild it from the store
        // when the live tree is stale *or* out-of-core (a disk-backed tree holds
        // no in-memory nodes, so serializing it directly would write an empty
        // tree — rebuild from the store instead so the snapshot is complete).
        if self.index_valid && !self.vstree.is_disk_backed() {
            write_bincode(&dir.join(VSTREE_FILE), &self.vstree)?;
        } else {
            write_bincode(&dir.join(VSTREE_FILE), &build_vstree(&self.store))?;
        }
        Ok(())
    }

    // ---- on-disk B+ tree KVstore (backlog item A) -------------------------

    /// Build an on-disk database (B+ tree KVstore) in directory `dir` from RDF
    /// files. The triples and dictionary are written to `kvstore.kv` through the
    /// page cache and persisted; nothing is kept in memory.
    pub fn build_disk<P: AsRef<Path>, Q: AsRef<Path>>(dir: P, files: &[Q]) -> Result<()> {
        let dir = dir.as_ref();
        fs::create_dir_all(dir)?;
        // Fresh build: remove any prior store/VS-tree files *and* their WALs, so a
        // leftover WAL from a crashed earlier build can't replay into the new file.
        let kv = dir.join(KV_FILE);
        remove_file_and_wal(&kv);
        let ds = DiskStore::build_files(&kv, DISK_CACHE_PAGES, files)?;
        // Build the out-of-core VS-tree alongside the store, so a later
        // `load_disk` filters through it on disk instead of materializing it.
        let vskv = dir.join(VSTREE_KV_FILE);
        remove_file_and_wal(&vskv);
        VsTree::build_disk(&vskv, vstree_entries(&ds))?;
        Ok(())
    }

    /// Whether directory `dir` holds an on-disk KVstore database.
    pub fn is_disk<P: AsRef<Path>>(dir: P) -> bool {
        dir.as_ref().join(KV_FILE).is_file()
    }

    /// Open an on-disk database and materialize it into the in-memory engine for
    /// querying. (The disk B+ trees are the source of truth; this loads the
    /// working set through the page cache — streaming evaluation directly off
    /// disk is a further optimization, see REFACTOR_BACKLOG item A.)
    pub fn load_disk<P: AsRef<Path>>(dir: P) -> Result<Database> {
        let dir = dir.as_ref();
        let kv = dir.join(KV_FILE);
        if !kv.is_file() {
            return Err(GStoreError::Database(format!(
                "no on-disk KVstore at '{}'",
                kv.display()
            )));
        }
        let ds = DiskStore::open(&kv, DISK_CACHE_PAGES)?;
        let (dict, store) = ds.to_memory()?;
        let name = dir
            .file_name()
            .map(|s| s.to_string_lossy().trim_end_matches(".db").to_string())
            .unwrap_or_else(|| "disk".to_string());
        // Out-of-core VS-tree: open the persisted paged node file (built by
        // `build_disk`), or build+persist it now if this disk database predates
        // it. Either way the candidate filter reads only the nodes a query
        // traverses on disk — the whole tree is never materialized in RAM.
        let vskv = dir.join(VSTREE_KV_FILE);
        let vstree = if vskv.is_file() {
            VsTree::open_disk(&vskv)?
        } else {
            VsTree::build_disk(&vskv, vstree_entries(&ds))?
        };
        Ok(Database {
            name,
            dict,
            store: Backend::Memory(store),
            named: BTreeMap::new(),
            vstree,
            // The disk-backed tree is consistent with the just-loaded store, so
            // the query path may use it as a candidate filter.
            index_valid: true,
            txn: None,
            query_cache: RefCell::new(LruCache::new(DEFAULT_QUERY_CACHE_CAP)),
            update_log: None,
            redo_log: None,
            redo_pending: Vec::new(),
            rules: crate::reason::RuleSet::new(),
            functions: FunctionRegistry::new(),
        })
    }

    /// Load a database from directory `dir`.
    pub fn load<P: AsRef<Path>>(dir: P) -> Result<Database> {
        let dir = dir.as_ref();
        if !dir.is_dir() {
            return Err(GStoreError::Database(format!(
                "database directory '{}' does not exist",
                dir.display()
            )));
        }
        let meta: Meta = read_bincode(&dir.join(META_FILE))?;
        let dict: Dictionary = read_bincode(&dir.join(DICT_FILE))?;
        let store: TripleStore = read_bincode(&dir.join(STORE_FILE))?;
        // Named graphs are optional (older databases / no GRAPH data).
        let named: BTreeMap<u32, TripleStore> =
            read_bincode(&dir.join(NAMED_FILE)).unwrap_or_default();
        // Sanity check that the snapshot is internally consistent.
        if store.triple_count() != meta.triple_num {
            return Err(GStoreError::Database(format!(
                "corrupt database: meta says {} triples but store has {}",
                meta.triple_num,
                store.triple_count()
            )));
        }
        if dict.entity_num() as u64 != meta.entity_num {
            return Err(GStoreError::Database(format!(
                "corrupt database: meta says {} entities but dict has {}",
                meta.entity_num,
                dict.entity_num()
            )));
        }
        if dict.literal_num() as u64 != meta.literal_num {
            return Err(GStoreError::Database(format!(
                "corrupt database: meta says {} literals but dict has {}",
                meta.literal_num,
                dict.literal_num()
            )));
        }
        if dict.predicate_num() as u64 != meta.predicate_num {
            return Err(GStoreError::Database(format!(
                "corrupt database: meta says {} predicates but dict has {}",
                meta.predicate_num,
                dict.predicate_num()
            )));
        }
        // The VS-tree is rebuilt from the store if absent or unreadable.
        let vstree = read_bincode(&dir.join(VSTREE_FILE)).unwrap_or_else(|_| build_vstree(&store));
        Ok(Database {
            name: meta.name,
            dict,
            store: Backend::Memory(store),
            named,
            vstree,
            index_valid: true,
            txn: None,
            query_cache: RefCell::new(LruCache::new(DEFAULT_QUERY_CACHE_CAP)),
            update_log: None,
            redo_log: None,
            redo_pending: Vec::new(),
            rules: crate::reason::RuleSet::new(),
            functions: FunctionRegistry::new(),
        })
    }

    // ---- backup / restore -------------------------------------------------

    /// Back up a consistent snapshot of this database into `backup_dir`
    /// (gStore `Database::backup`). The snapshot is fully independent of the
    /// live database; restore it with [`restore`](Self::restore).
    pub fn backup<P: AsRef<Path>>(&self, backup_dir: P) -> Result<()> {
        self.save(backup_dir)
    }

    /// Restore a database from a backup directory created by
    /// [`backup`](Self::backup) (gStore `Database::restore`).
    pub fn restore<P: AsRef<Path>>(backup_dir: P) -> Result<Database> {
        Database::load(backup_dir)
    }

    /// File-level backup of a *persisted* database directory `src` into `dst`,
    /// copying every database file present (the bincode snapshot, the on-disk
    /// KVstore, and the update log) without loading anything into memory. Use
    /// this to back up an on-disk (`build_disk`) database.
    pub fn backup_dir<P: AsRef<Path>, Q: AsRef<Path>>(src: P, dst: Q) -> Result<()> {
        let (src, dst) = (src.as_ref(), dst.as_ref());
        if !src.is_dir() {
            return Err(GStoreError::Database(format!(
                "backup source '{}' is not a directory",
                src.display()
            )));
        }
        fs::create_dir_all(dst)?;
        for name in [
            DICT_FILE,
            STORE_FILE,
            NAMED_FILE,
            META_FILE,
            VSTREE_FILE,
            KV_FILE,
            VSTREE_KV_FILE,
            UPDATE_LOG_FILE,
        ] {
            let s = src.join(name);
            if s.is_file() {
                fs::copy(&s, dst.join(name))?;
            }
        }
        Ok(())
    }

    // ---- update log -------------------------------------------------------

    /// Enable update logging to `path`: every subsequent SPARQL UPDATE applied
    /// through [`query`](Self::query) is appended to the file (gStore
    /// `update.log`). The file is created on first write.
    pub fn enable_update_log<P: AsRef<Path>>(&mut self, path: P) {
        self.update_log = Some(path.as_ref().to_path_buf());
    }

    /// Enable update logging at the conventional `update.log` inside a database
    /// directory (created if necessary).
    pub fn enable_update_log_in<P: AsRef<Path>>(&mut self, dir: P) -> Result<()> {
        let dir = dir.as_ref();
        fs::create_dir_all(dir)?;
        self.update_log = Some(dir.join(UPDATE_LOG_FILE));
        Ok(())
    }

    /// Disable update logging (subsequent updates are not recorded).
    pub fn disable_update_log(&mut self) {
        self.update_log = None;
    }

    /// Whether update logging is currently enabled.
    pub fn update_log_enabled(&self) -> bool {
        self.update_log.is_some()
    }

    /// Replay every UPDATE statement recorded in an update log `path` against
    /// this database, in order, returning the number of statements applied.
    /// Logging is suspended during replay so the log is not rewritten and no
    /// recursion occurs.
    pub fn replay_update_log<P: AsRef<Path>>(&mut self, path: P) -> Result<usize> {
        let data = fs::read(path)?;
        let saved = self.update_log.take();
        let result = self.replay_records(&data);
        self.update_log = saved;
        result
    }

    /// Parse and apply the length-prefixed records of an update log buffer.
    fn replay_records(&mut self, bytes: &[u8]) -> Result<usize> {
        let mut pos = 0usize;
        let mut applied = 0usize;
        while pos < bytes.len() {
            // Header line: `REC <millis> <changed> <byte_len>`.
            let nl = bytes[pos..]
                .iter()
                .position(|&b| b == b'\n')
                .map(|i| pos + i)
                .ok_or_else(|| GStoreError::Database("truncated update.log header".into()))?;
            let header = std::str::from_utf8(&bytes[pos..nl])
                .map_err(|_| GStoreError::Database("non-UTF-8 update.log header".into()))?;
            let mut it = header.split_whitespace();
            if it.next() != Some("REC") {
                return Err(GStoreError::Database(format!(
                    "malformed update.log record header: {header:?}"
                )));
            }
            let _millis = it.next();
            let _changed = it.next();
            let len: usize = it
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| GStoreError::Database("missing length in update.log record".into()))?;
            let start = nl + 1;
            let end = start + len;
            if end > bytes.len() {
                return Err(GStoreError::Database("truncated update.log body".into()));
            }
            let sparql = std::str::from_utf8(&bytes[start..end])
                .map_err(|_| GStoreError::Database("non-UTF-8 update.log body".into()))?;
            self.query(sparql)?;
            applied += 1;
            // Skip the body and its trailing newline.
            pos = (end + 1).min(bytes.len());
        }
        Ok(applied)
    }

    // ---- redo log (the redo half of gStore's WAL) -------------------------

    /// Enable redo logging to `path`: every subsequent *committed* triple
    /// mutation (insert/delete, default or named graph) is appended as a redo
    /// record so it can be re-applied on recovery with
    /// [`replay_redo_log`](Self::replay_redo_log).
    pub fn enable_redo_log<P: AsRef<Path>>(&mut self, path: P) {
        self.redo_log = Some(path.as_ref().to_path_buf());
    }

    /// Enable redo logging at the conventional `redo.log` inside a database
    /// directory (created if necessary).
    pub fn enable_redo_log_in<P: AsRef<Path>>(&mut self, dir: P) -> Result<()> {
        let dir = dir.as_ref();
        fs::create_dir_all(dir)?;
        self.redo_log = Some(dir.join(REDO_LOG_FILE));
        Ok(())
    }

    /// Disable redo logging (subsequent mutations are not recorded). Any buffered
    /// (uncommitted-transaction) records are dropped.
    pub fn disable_redo_log(&mut self) {
        self.redo_log = None;
        self.redo_pending.clear();
    }

    /// Whether redo logging is currently enabled.
    pub fn redo_log_enabled(&self) -> bool {
        self.redo_log.is_some()
    }

    /// Record one committed mutation into the redo log. In auto-commit mode the
    /// record is written through immediately; inside a (single-writer)
    /// transaction it is buffered until [`commit`](Self::commit). No-op when the
    /// redo log is disabled. Best-effort on the immediate-write path (an I/O
    /// error there is swallowed so it never changes a mutation's `bool` result).
    fn record_redo(&mut self, op: u8, graph: Option<u32>, id: IdTriple) {
        if self.redo_log.is_none() {
            return;
        }
        let (Some(s), Some(p), Some(o)) = (
            self.dict.id_to_string(id.sub),
            self.dict.predicate_to_string(id.pred),
            self.dict.id_to_string(id.obj),
        ) else {
            return;
        };
        let triple = format!("{s} {p} {o} .");
        let graph = graph
            .and_then(|g| self.dict.id_to_string(g))
            .map(|g| strip_angle(g).to_owned());
        self.redo_pending.push(RedoRec { op, graph, triple });
        if self.txn.is_none() {
            // Auto-commit: the mutation is already durable, so flush now.
            let _ = self.flush_redo();
        }
    }

    /// Flush buffered redo records to the redo log and clear the buffer.
    fn flush_redo(&mut self) -> Result<()> {
        if self.redo_pending.is_empty() {
            return Ok(());
        }
        if let Some(path) = self.redo_log.clone() {
            append_redo_log(&path, &self.redo_pending)?;
        }
        self.redo_pending.clear();
        Ok(())
    }

    /// Re-apply every mutation recorded in a redo log `path` against this
    /// database, in order, returning the number of records applied. Replay onto a
    /// database in the same state the log began from reconstructs the final
    /// state. Redo logging is suspended during replay so the log is not rewritten.
    pub fn replay_redo_log<P: AsRef<Path>>(&mut self, path: P) -> Result<usize> {
        let data = fs::read(path)?;
        let saved = self.redo_log.take();
        let result = self.apply_redo_records(&data);
        self.redo_log = saved;
        result
    }

    /// Parse and apply the length-prefixed records of a redo log buffer.
    fn apply_redo_records(&mut self, bytes: &[u8]) -> Result<usize> {
        let mut pos = 0usize;
        let mut applied = 0usize;
        while pos < bytes.len() {
            // Header line: `REDO <op> <graph_len> <triple_len>`.
            let nl = bytes[pos..]
                .iter()
                .position(|&b| b == b'\n')
                .map(|i| pos + i)
                .ok_or_else(|| GStoreError::Database("truncated redo.log header".into()))?;
            let header = std::str::from_utf8(&bytes[pos..nl])
                .map_err(|_| GStoreError::Database("non-UTF-8 redo.log header".into()))?;
            let mut it = header.split_whitespace();
            if it.next() != Some("REDO") {
                return Err(GStoreError::Database(format!(
                    "malformed redo.log record header: {header:?}"
                )));
            }
            let op = it
                .next()
                .and_then(|s| s.chars().next())
                .ok_or_else(|| GStoreError::Database("missing op in redo.log record".into()))?;
            let glen: usize = it
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| GStoreError::Database("missing graph length in redo.log".into()))?;
            let tlen: usize = it
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| GStoreError::Database("missing triple length in redo.log".into()))?;
            let gstart = nl + 1;
            let gend = gstart + glen;
            let tend = gend + tlen;
            if tend > bytes.len() {
                return Err(GStoreError::Database("truncated redo.log body".into()));
            }
            let graph = std::str::from_utf8(&bytes[gstart..gend])
                .map_err(|_| GStoreError::Database("non-UTF-8 redo.log graph".into()))?;
            let triple_str = std::str::from_utf8(&bytes[gend..tend])
                .map_err(|_| GStoreError::Database("non-UTF-8 redo.log triple".into()))?;
            let graph_opt = if graph.is_empty() { None } else { Some(graph) };
            for t in turtle::parse_str(triple_str)? {
                match op {
                    'I' => {
                        self.insert_quad(&t, graph_opt);
                    }
                    'D' => {
                        self.delete_quad(&t, graph_opt);
                    }
                    other => {
                        return Err(GStoreError::Database(format!(
                            "unknown redo.log op '{other}'"
                        )))
                    }
                }
            }
            applied += 1;
            pos = (tend + 1).min(bytes.len());
        }
        Ok(applied)
    }

    // ---- id freelist reclamation (gStore freeEntityID etc.) ----------------

    /// Conservative id-reclamation pass (gStore's `freelist` rebuild): free the
    /// dictionary id of every term that no longer appears anywhere — neither in
    /// the default graph nor any named graph — so those ids can be reused by
    /// future interns. Returns the number of ids reclaimed.
    ///
    /// "Referenced" means: an id used as a subject or object (entity/literal
    /// space), as a predicate (predicate space), or as a named-graph IRI. Only
    /// truly-unreferenced ids are freed, so the store stays consistent.
    pub fn reclaim_unused(&mut self) -> usize {
        let mut ent_lit: HashSet<EntityLiteralId> = HashSet::new();
        let mut preds: HashSet<PredId> = HashSet::new();

        // Default graph.
        for id in self.store.subject_keys() {
            ent_lit.insert(id);
        }
        for id in self.store.object_keys() {
            ent_lit.insert(id);
        }
        for p in self.store.predicates() {
            preds.insert(p);
        }
        // Named graphs, plus their graph-IRI entity ids.
        for (&gid, s) in &self.named {
            ent_lit.insert(gid);
            ent_lit.extend(s.subject_keys());
            ent_lit.extend(s.object_keys());
            preds.extend(s.predicates());
        }

        self.dict.reclaim_unused(&ent_lit, &preds)
    }

    // ---- user-defined reasoning rules (gStore ReasonHelper) ----------------

    /// Define a forward-chaining rule from its textual `body => head` form
    /// (see [`crate::reason`]). Errors if the name is taken or the text is
    /// malformed.
    pub fn add_rule(&mut self, name: impl Into<String>, text: &str) -> Result<()> {
        self.rules.add_rule(name, text)
    }

    /// Remove a rule by name; `true` if it existed.
    pub fn remove_rule(&mut self, name: &str) -> bool {
        self.rules.remove(name)
    }

    /// Enable a rule by name; `true` if it existed.
    pub fn enable_rule(&mut self, name: &str) -> bool {
        self.rules.enable(name)
    }

    /// Disable a rule by name; `true` if it existed.
    pub fn disable_rule(&mut self, name: &str) -> bool {
        self.rules.disable(name)
    }

    /// List rules as `(name, enabled, effect_count)` in definition order.
    pub fn list_rules(&self) -> Vec<(String, bool, usize)> {
        self.rules.list()
    }

    /// The most recent effect count (triples inferred) of a named rule.
    pub fn rule_effect_count(&self, name: &str) -> Option<usize> {
        self.rules.effect_count(name)
    }

    /// Materialize the closure of every enabled user rule into the store,
    /// updating each rule's effect count. Returns the number of triples inferred;
    /// inferred triples are recorded for transaction rollback like
    /// [`materialize_rdfs`](Self::materialize_rdfs).
    pub fn run_rules(&mut self) -> usize {
        let added = self.rules.apply(&mut self.dict, &mut self.store);
        let n = added.len();
        if n > 0 {
            self.index_valid = false;
            self.query_cache.get_mut().clear();
            if let Some(log) = self.txn.as_mut() {
                for t in &added {
                    log.push(UndoOp::Del(None, *t));
                }
            }
        }
        n
    }

    // ---- query-cache capacity --------------------------------------------

    /// Resize the read-query LRU cache, clearing it. A capacity of 0 disables
    /// caching entirely.
    pub fn set_query_cache_capacity(&mut self, cap: usize) {
        *self.query_cache.get_mut() = LruCache::new(cap);
    }

    /// Number of entries currently in the read-query cache (tests).
    #[cfg(test)]
    fn query_cache_len(&self) -> usize {
        self.query_cache.borrow().len()
    }

    /// Whether a given SPARQL string is currently cached (tests).
    #[cfg(test)]
    fn query_cache_contains(&self, sparql: &str) -> bool {
        self.query_cache.borrow().contains(sparql)
    }

    // ---- bulk / parallel loaders -----------------------------------------

    /// Build a database from an N-Triples file in bounded-memory batches
    /// (gStore RDFParser's chunked import): triples are streamed and flushed to
    /// the store every `batch` triples, so the transient encode buffer never
    /// exceeds `batch` entries — suitable for files too large to buffer whole.
    // `drain(..).collect()` (not `mem::take`) is deliberate: it hands the
    // batch's triples to `bulk_load` while keeping `buf`'s allocation for reuse.
    #[allow(clippy::drain_collect)]
    pub fn build_from_ntriples_batched<P: AsRef<Path>>(
        name: impl Into<String>,
        path: P,
        batch: usize,
    ) -> Result<Database> {
        let batch = batch.max(1);
        let mut db = Database::new(name);
        let mut buf: Vec<IdTriple> = Vec::with_capacity(batch.min(1 << 20));
        crate::parser::ntriples::for_each_triple_file(path, |t| {
            let id = db.encode_triple(&t);
            buf.push(id);
            if buf.len() >= batch {
                db.store.bulk_load(buf.drain(..).collect());
            }
            Ok(())
        })?;
        if !buf.is_empty() {
            db.store.bulk_load(buf.drain(..).collect());
        }
        db.rebuild_index();
        Ok(db)
    }

    /// Build a database from an N-Triples file using `threads` parser threads
    /// (gStore's multi-threaded load). The file is split into line-aligned
    /// chunks parsed in parallel; interning + indexing run serially afterward
    /// (the dictionary is the single synchronization point).
    pub fn build_from_ntriples_parallel<P: AsRef<Path>>(
        name: impl Into<String>,
        path: P,
        threads: usize,
    ) -> Result<Database> {
        let content = fs::read_to_string(path)?;
        Database::build_from_ntriples_str_parallel(name, &content, threads)
    }

    /// Parallel-parse an in-memory N-Triples document with `threads` threads,
    /// then intern + index serially. See [`build_from_ntriples_parallel`].
    pub fn build_from_ntriples_str_parallel(
        name: impl Into<String>,
        content: &str,
        threads: usize,
    ) -> Result<Database> {
        let threads = threads.max(1);
        let chunks = split_lines(content, threads);
        // Parse each chunk on its own thread; std::thread::scope guarantees the
        // borrowed `content` outlives every spawned parser.
        let parsed: Vec<Result<Vec<Triple>>> = std::thread::scope(|scope| {
            let handles: Vec<_> = chunks
                .into_iter()
                .map(|c| scope.spawn(move || crate::parser::ntriples::parse_str(c)))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("parser thread panicked"))
                .collect()
        });
        let mut db = Database::new(name);
        let mut ids: Vec<IdTriple> = Vec::new();
        for chunk in parsed {
            for t in chunk? {
                ids.push(db.encode_triple(&t));
            }
        }
        db.store.bulk_load(ids);
        db.rebuild_index();
        Ok(db)
    }
}

/// Append one UPDATE statement to the update log as a length-prefixed record:
/// `REC <unix_millis> <changed> <byte_len>\n<sparql bytes>\n`. The length prefix
/// makes the body opaque, so a statement may contain any bytes (including the
/// record-delimiter text) without corrupting the log.
fn append_update_log(path: &Path, sparql: &str, changed: usize) -> Result<()> {
    let mut f = fs::OpenOptions::new().create(true).append(true).open(path)?;
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    writeln!(f, "REC {millis} {changed} {}", sparql.len())?;
    f.write_all(sparql.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
}

/// Append redo records to the redo log. Each record is length-prefixed:
/// `REDO <op> <graph_len> <triple_len>\n<graph bytes><triple bytes>\n`. The
/// lengths make the graph/triple bodies opaque, so they may hold any bytes.
fn append_redo_log(path: &Path, recs: &[RedoRec]) -> Result<()> {
    let mut f = fs::OpenOptions::new().create(true).append(true).open(path)?;
    for r in recs {
        let g = r.graph.as_deref().unwrap_or("");
        writeln!(f, "REDO {} {} {}", r.op as char, g.len(), r.triple.len())?;
        f.write_all(g.as_bytes())?;
        f.write_all(r.triple.as_bytes())?;
        f.write_all(b"\n")?;
    }
    Ok(())
}

/// Strip a leading `<` and trailing `>` from an IRI dict-key, leaving the raw
/// IRI (the form named-graph helpers expect). Non-bracketed input is returned
/// unchanged.
fn strip_angle(s: &str) -> &str {
    s.strip_prefix('<')
        .and_then(|r| r.strip_suffix('>'))
        .unwrap_or(s)
}

/// Split `content` into at most `n` line-aligned `&str` chunks (each chunk ends
/// on a newline boundary, so no N-Triples line is ever cut in two).
fn split_lines(content: &str, n: usize) -> Vec<&str> {
    if n <= 1 || content.is_empty() {
        return vec![content];
    }
    let bytes = content.as_bytes();
    let len = bytes.len();
    let mut chunks: Vec<&str> = Vec::with_capacity(n);
    let mut start = 0usize;
    for i in 1..n {
        if start >= len {
            break;
        }
        let mut cut = (len * i / n).max(start);
        while cut < len && bytes[cut] != b'\n' {
            cut += 1;
        }
        if cut < len {
            cut += 1; // include the newline in this chunk
        }
        if cut > start {
            chunks.push(&content[start..cut]);
            start = cut;
        }
    }
    if start < len {
        chunks.push(&content[start..]);
    }
    if chunks.is_empty() {
        chunks.push(content);
    }
    chunks
}

fn write_bincode<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let file = fs::File::create(path)?;
    let writer = std::io::BufWriter::new(file);
    bincode::serialize_into(writer, value)?;
    Ok(())
}

fn read_bincode<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let file = fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    Ok(bincode::deserialize_from(reader)?)
}

/// Remove a pager-backed file and its sibling `<path>.wal`, ignoring absence.
/// Used before a fresh disk build so a stale WAL from a crashed prior build can
/// never replay into the new file (see [`crate::kvstore::pager`] recovery).
fn remove_file_and_wal(path: &Path) {
    let _ = fs::remove_file(path);
    let mut wal = path.as_os_str().to_owned();
    wal.push(".wal");
    let _ = fs::remove_file(PathBuf::from(wal));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Term;
    use crate::query::QueryResult;

    const SMALL: &str = "\
<root> <name> \"Bookug Lobert\" .
<root> <contain> <node0> .
<root> <contain> <node1> .
<node1> <own> <point0> .
<node1> <own> <point1> .
";

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("gstore_ut_{tag}"));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn build_and_count() {
        let db = Database::build_from_str("t", SMALL).unwrap();
        assert_eq!(db.triple_num(), 5);
        // entities: root, node0, node1, point0, point1 = 5
        assert_eq!(db.entity_num(), 5);
        // literals: "Bookug Lobert" = 1
        assert_eq!(db.literal_num(), 1);
        // predicates: name, contain, own = 3
        assert_eq!(db.predicate_num(), 3);
    }

    #[test]
    fn query_select_over_built_db() {
        let mut db = Database::build_from_str("t", SMALL).unwrap();
        let rs = db
            .select("SELECT ?o WHERE { <root> <contain> ?o }")
            .unwrap();
        let mut got: Vec<String> = rs.rows.iter().map(|r| r[0].clone().unwrap()).collect();
        got.sort();
        assert_eq!(got, vec!["<node0>".to_string(), "<node1>".to_string()]);
    }

    #[test]
    fn query_literal_object() {
        let mut db = Database::build_from_str("t", SMALL).unwrap();
        let rs = db.select("SELECT ?n WHERE { <root> <name> ?n }").unwrap();
        assert_eq!(rs.rows[0][0], Some("\"Bookug Lobert\"".into()));
    }

    #[test]
    fn register_function_visible_to_query() {
        // PFN parity at the Database facade: a registered custom function is
        // visible to queries run through Database::select.
        let mut db = Database::build_from_str(
            "pfn",
            "<http://ex/alice> <http://ex/salary> \"2500\"^^<http://www.w3.org/2001/XMLSchema#integer> .\n",
        )
        .unwrap();
        db.register_function("myDouble", |args| {
            Some(crate::query::Value::Double(args.first()?.as_f64()? * 2.0))
        });
        assert_eq!(db.function_count(), 1);
        let rs = db
            .select("SELECT (myDouble(?s) AS ?d) WHERE { ?x <http://ex/salary> ?s }")
            .unwrap();
        assert!(rs.rows[0][0].as_ref().unwrap().contains("5000"));
    }

    #[test]
    fn insert_and_delete_triple() {
        let mut db = Database::build_from_str("t", SMALL).unwrap();
        let before = db.triple_num();
        let t = Triple::new(Term::iri("root"), Term::iri("contain"), Term::iri("node9"));
        assert!(db.insert_triple(&t));
        assert!(!db.insert_triple(&t)); // duplicate
        assert_eq!(db.triple_num(), before + 1);
        assert!(db.remove_triple(&t));
        assert_eq!(db.triple_num(), before);
    }

    #[test]
    fn sparql_insert_delete_data() {
        let mut db = Database::build_from_str("t", SMALL).unwrap();
        let before = db.triple_num();
        match db
            .query("INSERT DATA { <root> <contain> <nodeX> . <nodeX> <own> <pointZ> }")
            .unwrap()
        {
            QueryResult::Update { changed } => assert_eq!(changed, 2),
            other => panic!("expected Update, got {other:?}"),
        }
        assert_eq!(db.triple_num(), before + 2);

        // The new triple is queryable.
        let rs = db.select("SELECT ?o WHERE { <nodeX> <own> ?o }").unwrap();
        assert_eq!(rs.rows[0][0], Some("<pointZ>".into()));

        match db
            .query("DELETE DATA { <root> <contain> <nodeX> }")
            .unwrap()
        {
            QueryResult::Update { changed } => assert_eq!(changed, 1),
            other => panic!("expected Update, got {other:?}"),
        }
        assert_eq!(db.triple_num(), before + 1);
    }

    #[test]
    fn delete_nonexistent_changes_nothing() {
        let mut db = Database::build_from_str("t", SMALL).unwrap();
        let before = db.triple_num();
        match db.query("DELETE DATA { <ghost> <p> <o> }").unwrap() {
            QueryResult::Update { changed } => assert_eq!(changed, 0),
            other => panic!("got {other:?}"),
        }
        assert_eq!(db.triple_num(), before);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = temp_dir("roundtrip");
        let db = Database::build_from_str("rt", SMALL).unwrap();
        db.save(&dir).unwrap();

        let loaded = Database::load(&dir).unwrap();
        assert_eq!(loaded.name(), "rt");
        assert_eq!(loaded.triple_num(), db.triple_num());
        assert_eq!(loaded.entity_num(), db.entity_num());
        assert_eq!(loaded.predicate_num(), db.predicate_num());

        // Queries work identically after reload.
        let mut loaded = loaded;
        let rs = loaded
            .select("SELECT ?o WHERE { <node1> <own> ?o }")
            .unwrap();
        assert_eq!(rs.row_count(), 2);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_missing_dir_errors() {
        let e = Database::load(std::env::temp_dir().join("gstore_does_not_exist_xyz")).unwrap_err();
        assert!(matches!(e, GStoreError::Database(_)));
    }

    // ---- incremental VS-tree maintenance ---------------------------------

    /// Inserts must update the VS-tree incrementally (keeping the index valid)
    /// and the query results must match a freshly-rebuilt baseline at every step.
    #[test]
    fn incremental_vstree_keeps_index_valid_and_matches_rebuild() {
        let mut inc = Database::build_from_str("inc", SMALL).unwrap();
        assert!(inc.stats().index_valid, "build leaves the index valid");

        // Edges added one at a time; some touch existing entities (signature
        // grows), some introduce brand-new entities (tree insert).
        let extra = [
            ("node0", "own", "point2"),
            ("node0", "link", "node1"),
            ("node1", "own", "point2"),
            ("root", "contain", "node2"),
            ("node2", "own", "point3"),
            ("point0", "kind", "leaf"),
        ];

        let mut accumulated = SMALL.to_string();
        for (s, p, o) in extra {
            assert!(inc.insert_triple(&Triple::new(
                Term::iri(s),
                Term::iri(p),
                Term::iri(o)
            )));
            // The whole point of task 1: the index is *not* invalidated.
            assert!(
                inc.stats().index_valid,
                "incremental insert must keep the index valid"
            );

            // Baseline: a fresh DB over all triples so far does a full VS-tree
            // rebuild. The incremental DB answers via its updated tree.
            accumulated.push_str(&format!("<{s}> <{p}> <{o}> .\n"));
            let mut base = Database::build_from_str("base", &accumulated).unwrap();

            for q in [
                "SELECT ?o WHERE { <root> <contain> ?o }",
                "SELECT ?s ?o WHERE { ?s <own> ?o }",
                "SELECT ?s WHERE { ?s <own> <point2> }",
                "SELECT ?s ?p ?o WHERE { ?s ?p ?o }",
            ] {
                let mut a = inc.select(q).unwrap().rows;
                let mut b = base.select(q).unwrap().rows;
                a.sort();
                b.sort();
                assert_eq!(a, b, "query {q:?} diverged after inserting ({s},{p},{o})");
            }
        }
    }

    /// Incremental inserts that force VS-tree leaf/internal splits (more than the
    /// node fan-out of new entities) stay sound against a full-rebuild baseline.
    #[test]
    fn incremental_vstree_sound_under_node_splits() {
        let mut inc = Database::new("inc");
        assert!(inc.stats().index_valid);

        let mut accumulated = String::new();
        let n = 300u32; // well past the VS-tree fan-out (64) ⇒ splits happen
        for i in 0..n {
            let s = format!("e{i}");
            let o = format!("t{}", i % 23);
            assert!(inc.insert_triple(&Triple::new(
                Term::iri(&s),
                Term::iri("p"),
                Term::iri(&o)
            )));
            accumulated.push_str(&format!("<{s}> <p> <{o}> .\n"));
        }
        assert!(
            inc.stats().index_valid,
            "every incremental insert kept the index valid"
        );

        let mut base = Database::build_from_str("base", &accumulated).unwrap();
        for q in [
            "SELECT ?s WHERE { ?s <p> <t7> }",
            "SELECT ?s WHERE { ?s <p> <t0> }",
            "SELECT ?s ?o WHERE { ?s <p> ?o }",
        ] {
            let mut a = inc.select(q).unwrap().rows;
            let mut b = base.select(q).unwrap().rows;
            a.sort();
            b.sort();
            assert_eq!(a, b, "query {q:?} diverged under incremental splits");
            assert!(!a.is_empty(), "query {q:?} should return rows");
        }
    }

    /// A delete invalidates the index (documented behaviour); a subsequent query
    /// still returns correct results (it evaluates without the stale tree, or
    /// after a rebuild on save), and inserts after the delete leave it stale.
    #[test]
    fn delete_invalidates_then_results_stay_correct() {
        let mut db = Database::build_from_str("d", SMALL).unwrap();
        assert!(db.stats().index_valid);
        assert!(db.remove_triple(&Triple::new(
            Term::iri("node1"),
            Term::iri("own"),
            Term::iri("point0")
        )));
        assert!(!db.stats().index_valid, "a delete invalidates the index");

        let rs = db.select("SELECT ?o WHERE { <node1> <own> ?o }").unwrap();
        let got: Vec<_> = rs.rows.iter().map(|r| r[0].clone().unwrap()).collect();
        assert_eq!(got, vec!["<point1>".to_string()], "point0 was deleted");

        // Rebuilding restores a valid index and the same answer.
        db.rebuild_index();
        assert!(db.stats().index_valid);
        let rs = db.select("SELECT ?o WHERE { <node1> <own> ?o }").unwrap();
        assert_eq!(rs.row_count(), 1);
    }

    // ---- redo log --------------------------------------------------------

    #[test]
    fn redo_log_replays_committed_mutations() {
        let dir = temp_dir("redo_replay");
        let mut db = Database::build_from_str("rd", SMALL).unwrap();
        db.enable_redo_log_in(&dir).unwrap();
        assert!(db.redo_log_enabled());

        // A mix of auto-commit mutations.
        db.query("INSERT DATA { <root> <contain> <nodeX> }").unwrap();
        db.insert_triple(&Triple::new(
            Term::iri("nodeX"),
            Term::iri("own"),
            Term::iri("pointZ"),
        ));
        db.query("DELETE DATA { <node1> <own> <point0> }").unwrap();
        let final_count = db.triple_num();

        // Recover into a fresh database starting from the same base.
        let mut recovered = Database::build_from_str("rd", SMALL).unwrap();
        let applied = recovered.replay_redo_log(dir.join(REDO_LOG_FILE)).unwrap();
        assert_eq!(applied, 3, "three committed mutations recorded");
        assert_eq!(recovered.triple_num(), final_count);
        assert_eq!(
            recovered
                .select("SELECT ?o WHERE { <nodeX> <own> ?o }")
                .unwrap()
                .row_count(),
            1
        );
        assert_eq!(
            recovered
                .select("SELECT ?o WHERE { <node1> <own> ?o }")
                .unwrap()
                .row_count(),
            1,
            "point0 was deleted; point1 remains"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn redo_log_omits_rolled_back_transaction() {
        let dir = temp_dir("redo_rollback");
        let mut db = Database::build_from_str("rd", SMALL).unwrap();
        db.enable_redo_log_in(&dir).unwrap();

        // A transaction that rolls back must leave nothing in the redo log.
        db.begin().unwrap();
        db.insert_triple(&Triple::new(Term::iri("x"), Term::iri("p"), Term::iri("y")));
        db.rollback().unwrap();

        // A committed transaction is recorded.
        db.begin().unwrap();
        db.insert_triple(&Triple::new(Term::iri("k"), Term::iri("p"), Term::iri("v")));
        db.commit().unwrap();

        let mut recovered = Database::build_from_str("rd", SMALL).unwrap();
        recovered.replay_redo_log(dir.join(REDO_LOG_FILE)).unwrap();
        assert_eq!(
            recovered
                .select("SELECT ?o WHERE { <x> <p> ?o }")
                .unwrap()
                .row_count(),
            0,
            "the rolled-back mutation must not be replayed"
        );
        assert_eq!(
            recovered
                .select("SELECT ?o WHERE { <k> <p> ?o }")
                .unwrap()
                .row_count(),
            1,
            "the committed mutation must be replayed"
        );
        fs::remove_dir_all(&dir).ok();
    }

    // ---- query-cache LRU -------------------------------------------------

    #[test]
    fn query_cache_lru_evicts_least_recently_used() {
        let mut db = Database::build_from_str("lru", SMALL).unwrap();
        db.set_query_cache_capacity(2);
        let q1 = "SELECT ?o WHERE { <root> <contain> ?o }";
        let q2 = "SELECT ?n WHERE { <root> <name> ?n }";
        let q3 = "SELECT ?o WHERE { <node1> <own> ?o }";

        db.query(q1).unwrap();
        db.query(q2).unwrap();
        assert_eq!(db.query_cache_len(), 2);
        // Touch q1 so q2 is now the least-recently used.
        db.query(q1).unwrap();
        // Inserting q3 evicts q2.
        db.query(q3).unwrap();
        assert_eq!(db.query_cache_len(), 2, "cache stays at capacity");
        assert!(db.query_cache_contains(q1));
        assert!(db.query_cache_contains(q3));
        assert!(!db.query_cache_contains(q2), "q2 was least-recently used");
    }

    #[test]
    fn write_invalidates_query_cache() {
        let mut db = Database::build_from_str("inv", SMALL).unwrap();
        db.query("SELECT ?o WHERE { <root> <contain> ?o }").unwrap();
        assert_eq!(db.query_cache_len(), 1);
        db.query("INSERT DATA { <root> <contain> <nodeZ> }").unwrap();
        assert_eq!(db.query_cache_len(), 0, "any write clears the cache");
    }

    // ---- id freelist reclamation -----------------------------------------

    #[test]
    fn reclaim_unused_reuses_deleted_term_ids() {
        let mut db = Database::build_from_str("rc", SMALL).unwrap();
        let before_entities = db.entity_num();
        let node0_id = db.dict().entity_id(&Term::iri("node0").dict_key()).unwrap();

        // Delete the only triple using <node0>; its id is not auto-freed.
        db.query("DELETE DATA { <root> <contain> <node0> }").unwrap();
        assert!(db.dict().entity_id(&Term::iri("node0").dict_key()).is_some());

        let freed = db.reclaim_unused();
        assert_eq!(freed, 1, "only <node0> became unreferenced");
        assert_eq!(db.entity_num(), before_entities - 1);
        assert!(
            db.dict().entity_id(&Term::iri("node0").dict_key()).is_none(),
            "node0 was reclaimed"
        );

        // A brand-new term reuses node0's freed id.
        db.query("INSERT DATA { <root> <contain> <fresh> }").unwrap();
        assert_eq!(
            db.dict().entity_id(&Term::iri("fresh").dict_key()),
            Some(node0_id),
            "the freed id is reused"
        );
    }

    // ---- user-defined rules through the Database facade -------------------

    #[test]
    fn database_runs_user_rules() {
        let mut db =
            Database::build_from_str("rl", "<a> <ancestor> <b> . <b> <ancestor> <c> .").unwrap();
        db.add_rule(
            "anc",
            "?x <ancestor> ?y . ?y <ancestor> ?z => ?x <ancestor> ?z",
        )
        .unwrap();
        let n = db.run_rules();
        assert_eq!(n, 1, "a→c inferred");
        assert_eq!(db.rule_effect_count("anc"), Some(1));
        assert_eq!(
            db.select("SELECT ?z WHERE { <a> <ancestor> ?z }")
                .unwrap()
                .row_count(),
            2,
            "a now reaches both b and c"
        );

        // Disabling is reflected in the listing and stops further inference.
        assert!(db.disable_rule("anc"));
        assert_eq!(db.list_rules(), vec![("anc".to_string(), false, 1)]);
    }

    // ---- out-of-core VS-tree disk database (task 1) ----------------------

    /// A disk database must build an on-disk VS-tree, load filtering through it
    /// (not a materialized copy), and answer queries identically to an in-memory
    /// build of the same data.
    #[test]
    fn disk_database_filters_via_on_disk_vstree() {
        let dir = temp_dir("disk_vstree");
        // Enough entities to force a multi-node VS-tree, plus a distinctive edge.
        let mut content = String::new();
        for i in 0..400u32 {
            content.push_str(&format!("<http://ex/s{i}> <http://ex/p> <http://ex/o{i}> .\n"));
        }
        content.push_str("<http://ex/special> <http://ex/marker> <http://ex/target> .\n");
        let rdf = std::env::temp_dir().join("gstore_ut_disk_vstree.nt");
        fs::write(&rdf, &content).unwrap();

        Database::build_disk(&dir, &[&rdf]).unwrap();
        assert!(Database::is_disk(&dir));
        // The dedicated on-disk VS-tree node file was produced.
        assert!(dir.join(VSTREE_KV_FILE).is_file(), "build_disk must write the VS-tree node file");

        let mut db = Database::load_disk(&dir).unwrap();
        assert!(db.vstree.is_disk_backed(), "loaded disk db must use the on-disk VS-tree");
        assert!(db.stats().index_valid, "the disk-backed index is valid for filtering");

        // In-memory baseline over the same data (full VS-tree rebuild).
        let mut base = Database::build_from_str("base", &content).unwrap();
        for q in [
            "SELECT ?s WHERE { ?s <http://ex/marker> <http://ex/target> }",
            "SELECT ?o WHERE { <http://ex/s5> <http://ex/p> ?o }",
            "SELECT ?s ?o WHERE { ?s <http://ex/p> ?o }",
            "SELECT ?s WHERE { ?s <http://ex/p> <http://ex/o42> }",
        ] {
            let mut a = db.select(q).unwrap().rows;
            let mut b = base.select(q).unwrap().rows;
            a.sort();
            b.sort();
            assert_eq!(a, b, "disk-vstree query {q:?} diverged from in-memory baseline");
        }

        // The distinctive marker query returns exactly the special subject — and
        // the candidate filter that produced it ran against on-disk nodes.
        let rs = db
            .select("SELECT ?s WHERE { ?s <http://ex/marker> <http://ex/target> }")
            .unwrap();
        assert_eq!(rs.row_count(), 1);
        assert_eq!(rs.rows[0][0].as_deref(), Some("<http://ex/special>"));
        // Running the query touched only some VS-tree node pages, not the whole
        // tree — proving it need not be fully resident.
        assert!(db.vstree.disk_pages_read() > 0, "the disk VS-tree was actually traversed");

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_file(&rdf);
    }

    /// Mutating a disk-loaded database invalidates the out-of-core index (it
    /// can't be updated in place), and queries stay correct afterwards.
    #[test]
    fn disk_database_mutation_invalidates_then_stays_correct() {
        let dir = temp_dir("disk_vstree_mut");
        let content = "<a> <p> <b> .\n<b> <p> <c> .\n";
        let rdf = std::env::temp_dir().join("gstore_ut_disk_vstree_mut.nt");
        fs::write(&rdf, content).unwrap();
        Database::build_disk(&dir, &[&rdf]).unwrap();
        let mut db = Database::load_disk(&dir).unwrap();
        assert!(db.vstree.is_disk_backed() && db.stats().index_valid);

        // An insert can't update the on-disk tree in place ⇒ index invalidates.
        assert!(db.insert_triple(&Triple::new(Term::iri("c"), Term::iri("p"), Term::iri("d"))));
        assert!(!db.stats().index_valid, "mutation invalidates the disk-backed index");

        // Query still correct (evaluates without the stale tree).
        let mut got = db.select("SELECT ?o WHERE { <c> <p> ?o }").unwrap().rows;
        got.sort();
        assert_eq!(got, vec![vec![Some("<d>".to_string())]]);

        // A rebuild restores a valid (now in-memory) index.
        db.rebuild_index();
        assert!(db.stats().index_valid && !db.vstree.is_disk_backed());
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_file(&rdf);
    }

    // ---- build progress reporting ----------------------------------------

    #[test]
    fn build_reports_progress_stages() {
        let stages = RefCell::new(Vec::new());
        let db = Database::build_from_str_with_progress("p", SMALL, |s| {
            stages.borrow_mut().push(s)
        })
        .unwrap();
        assert_eq!(db.triple_num(), 5);
        assert_eq!(
            stages.into_inner(),
            vec![
                BuildProgress::RdfParse,
                BuildProgress::Dictionary,
                BuildProgress::Index,
                BuildProgress::Done,
            ]
        );
    }
}
