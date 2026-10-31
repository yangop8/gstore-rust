//! The [`Database`] facade.
//!
//! Corresponds to gStore's `Database` class: it owns the dictionary and the
//! triple store, builds from RDF files, applies updates, answers SPARQL, and
//! persists to / loads from a database directory.
//!
//! Persistence (DESIGN §7): a database is a directory in one of three layouts
//! (see [`StorageKind`] and the `persist` submodule) — a bincode snapshot
//! (`dict.bin`, `store.bin`, `named.bin`, `meta.bin`, `vstree.bin`, written as
//! one crash-atomic set), the on-disk B+ tree KVstore (`kvstore.kv`, updated by
//! write-through), or a RocksDB store (`rocksdb/`, feature `rocksdb`).
//! [`Database::open`] detects the layout.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::analytics::csr::{CsrCache, CsrGraph};
use crate::dict::Dictionary;
use crate::error::{GStoreError, Result};
use crate::model::id::{is_entity_id, EntityLiteralId, PredId};
use crate::model::{IdTriple, Term, Triple};
use crate::parser::sparql::ast::{
    GraphPattern, GraphTarget, GroundTriple, QuadPattern, Query, UpdateOp, RDF_TYPE,
};
use crate::parser::{sparql, turtle};
use crate::query::{Evaluator, FunctionRegistry, IntegerDivision, QueryResult, TopKOptions};
use crate::signature::{EdgeDir, Signature, VsTree};
use crate::store::{Backend, MutableStore, TripleSource, TripleStore};

pub(crate) mod fsutil;
mod admin;
pub mod dirlock;
mod persist;

pub use admin::{SchemaEdge, SchemaGraph, SchemaNode};
pub use persist::StorageKind;

#[cfg(test)]
mod hardening_tests;
#[cfg(test)]
mod review_tests;
mod load_policy;
pub use load_policy::{LoadPolicy, LoadSource};
mod query_cache;
use query_cache::QueryCache;
pub use query_cache::DEFAULT_QUERY_CACHE_BYTES;

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
/// Marker file inside a *full* backup directory recording the update-log
/// checkpoint (byte length) captured at backup time. Lets a later incremental
/// backup know where its delta begins; ignored by [`Database::load`].
const BACKUP_OFFSET_FILE: &str = "BACKUP_OFFSET";
/// Marker file inside an *incremental* backup directory describing its delta
/// (the base/end update-log byte offsets it covers).
const INCREMENTAL_MARKER_FILE: &str = "INCREMENTAL";
/// The copied slice of the update log inside an incremental backup directory —
/// the raw length-prefixed UPDATE records for `[base_offset, end_offset)`.
const INCREMENTAL_LOG_FILE: &str = "incr.log";
/// The append-only triple-level redo log inside a database directory. Records
/// every *committed* mutation so it can be re-applied on recovery (the redo half
/// of gStore's WAL; the undo half is the per-transaction [`UndoOp`] log).
const REDO_LOG_FILE: &str = "redo.log";
/// The on-disk B+ tree KVstore file inside a database directory.
const KV_FILE: &str = "kvstore.kv";
/// The on-disk (out-of-core) VS-tree node file inside a disk database directory.
/// Distinct from the in-memory snapshot's `vstree.bin`: this is the paged node
/// file a disk database filters through without loading the whole tree.
const VSTREE_KV_FILE: &str = "vstree.kv";
/// Sub-directory holding the RocksDB store of a RocksDB-backed database
/// (detected with or without the `rocksdb` feature).
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
    /// Byte-bounded LRU cache of read-query results keyed by the SPARQL
    /// string (gStore `QueryCache`; see the `query_cache` module), cleared on
    /// any store mutation. Interior mutability so a `&self`/`&mut self` query
    /// path can read and populate it.
    query_cache: RefCell<QueryCache>,
    /// How `/` divides two integers in this database's queries (see
    /// [`set_integer_division`](Self::set_integer_division)).
    integer_division: IntegerDivision,
    /// Which `LIMIT` / `ORDER BY` / `ASK` optimizations (Top-K, early
    /// termination, DP-B) this database's queries may use (see
    /// [`set_topk_options`](Self::set_topk_options)); all on by default.
    topk: TopKOptions,
    /// How many bytes of JSON one graph-function call may produce (see
    /// [`set_gfun_max_output_bytes`](Self::set_gfun_max_output_bytes)).
    gfun_max_output_bytes: usize,
    /// Whose PFN functions this database's queries call, from which registry
    /// (see [`set_pfn_caller`](Self::set_pfn_caller)); a server passes the
    /// caller per query instead ([`query_as`](Self::query_as)).
    pfn: Option<crate::pfn::PfnCaller>,
    /// The sandbox limits PFN calls run under (see
    /// [`set_pfn_limits`](Self::set_pfn_limits)).
    pfn_limits: crate::pfn::PfnLimits,
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
    /// Incremental-backup checkpoint: the update-log byte offset the most recent
    /// full ([`backup`](Self::backup)) or incremental
    /// ([`backup_incremental`](Self::backup_incremental)) backup ended at. The
    /// next incremental captures update-log records from here onward, so
    /// successive incrementals chain contiguously. Interior-mutable because both
    /// backup methods take `&self`.
    backup_offset: Cell<u64>,
    /// User-defined forward-chaining reasoning rules (the crate's textual
    /// `body => head` rules, see [`crate::reason::RuleSet`]).
    rules: crate::reason::RuleSet,
    /// Where upstream-format reasoning rules (`reasonManage`, see
    /// [`reason_add`](Self::reason_add)) are kept: a directory override, or
    /// memory for a database without a directory.
    reason_home: crate::reason::store::RuleHome,
    /// User-defined scalar SPARQL functions (gStore's PFN, as a safe in-process
    /// registry rather than dlopen `.so` plugins). Passed to every read-query
    /// evaluator; consulted for any function name that is not a built-in.
    functions: FunctionRegistry,
    /// The directory this database was opened from / built into (`None` for a
    /// database that only ever lived in memory). Persistent layouts write their
    /// committed updates here.
    home: Option<PathBuf>,
    /// Write-through state of an on-disk (`kvstore.kv`) database: every
    /// committed mutation of the in-memory working set is mirrored to the
    /// [`DiskStore`](crate::kvstore::DiskStore). A `RefCell` so `save(&self)`
    /// can commit it.
    disk: RefCell<Option<persist::DiskSink>>,
    /// The first persistence failure (RocksDB dictionary / named-graph write)
    /// since the last commit point, reported by the next one.
    persist_error: RefCell<Option<String>>,
    /// Which sources SPARQL `LOAD` may read (see [`LoadPolicy`]). Permissive by
    /// default; network-facing servers install [`LoadPolicy::restricted`].
    load_policy: LoadPolicy,
    /// Advances on every applied data mutation (see
    /// [`mutation_count`](Self::mutation_count)).
    mutations: u64,
    /// The per-predicate CSR of the default graph used by the advanced graph
    /// functions (upstream `GeneralEvaluation::csr`), built lazily and keyed
    /// by [`mutations`](Self::mutations) so any data change invalidates it
    /// (see [`csr`](Self::csr)).
    csr_cache: CsrCache,
    /// RocksDB: dictionary terms known in memory whose write to the `dict` CF
    /// failed (e.g. interned by a reasoner run whose term write failed). The
    /// next insert that uses one writes it first, so no persisted triple ever
    /// references an id RocksDB has no dictionary entry for.
    #[cfg(feature = "rocksdb")]
    unpersisted_terms: HashSet<(crate::backend::rocks::TermSpace, u32)>,
    /// The inter-process lock on the directory this database is open from
    /// (see [`dirlock`]); declared last so it is released only after every
    /// other field (the on-disk store included) has been dropped.
    dir_lock: Option<dirlock::DirLock>,
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

/// The graph-to-graph UPDATE operations (SPARQL 1.1 Update §3.2.3-3.2.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GraphTransfer {
    Add,
    Move,
    Copy,
}

impl GraphTransfer {
    fn keyword(self) -> &'static str {
        match self {
            GraphTransfer::Add => "ADD",
            GraphTransfer::Move => "MOVE",
            GraphTransfer::Copy => "COPY",
        }
    }
}

/// The query-evaluation settings a server applies to every database it opens,
/// builds, creates or restores ([`Database::apply_query_settings`]); `gserver`
/// reads them from `conf.ini`'s `[query]` section
/// ([`GStoreConfig::query_settings`](crate::config::GStoreConfig::query_settings)).
/// [`Default`] is what a database built in-process starts with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuerySettings {
    /// How `/` divides two integers ([`Database::set_integer_division`];
    /// `integer_division`).
    pub integer_division: IntegerDivision,
    /// The read-query cache's budget in bytes, 0 = off
    /// ([`Database::set_query_cache_size`]; `query_cache_size`).
    pub query_cache_bytes: usize,
    /// The `LIMIT` / `ORDER BY` / `ASK` optimizations
    /// ([`Database::set_topk_options`]; `early_termination`,
    /// `topk_bounded_heap`, `topk_dpb`).
    pub topk: TopKOptions,
    /// The most bytes of JSON one graph-function call may produce
    /// ([`Database::set_gfun_max_output_bytes`]; `gfun_max_output_bytes`).
    /// Also bounds what one PFN call may emit.
    pub gfun_max_output_bytes: usize,
    /// The sandbox limits of PFN functions ([`Database::set_pfn_limits`];
    /// `conf.ini`'s `[pfn]` section).
    pub pfn: crate::pfn::PfnLimits,
}

impl Default for QuerySettings {
    fn default() -> QuerySettings {
        QuerySettings {
            integer_division: IntegerDivision::default(),
            query_cache_bytes: DEFAULT_QUERY_CACHE_BYTES,
            topk: TopKOptions::default(),
            gfun_max_output_bytes: crate::analytics::gfun::paths::MAX_OUTPUT_BYTES,
            pfn: crate::pfn::PfnLimits::default(),
        }
    }
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
            query_cache: RefCell::new(QueryCache::new(DEFAULT_QUERY_CACHE_BYTES)),
            integer_division: IntegerDivision::default(),
            topk: TopKOptions::default(),
            gfun_max_output_bytes: crate::analytics::gfun::paths::MAX_OUTPUT_BYTES,
            pfn: None,
            pfn_limits: crate::pfn::PfnLimits::default(),
            update_log: None,
            redo_log: None,
            redo_pending: Vec::new(),
            backup_offset: Cell::new(0),
            rules: crate::reason::RuleSet::new(),
            reason_home: crate::reason::store::RuleHome::default(),
            functions: FunctionRegistry::new(),
            home: None,
            disk: RefCell::new(None),
            persist_error: RefCell::new(None),
            load_policy: LoadPolicy::default(),
            mutations: 0,
            csr_cache: CsrCache::new(),
            #[cfg(feature = "rocksdb")]
            unpersisted_terms: HashSet::new(),
            dir_lock: None,
        }
    }

    /// A counter that advances whenever this database's data changes — every
    /// applied insert, delete, clear, inference and rollback; reads never move
    /// it. Comparing it before and after a request tells whether the request
    /// changed anything, *including* an UPDATE that failed part-way: its
    /// operations before the failing one stay applied (see
    /// [`query`](Self::query)), so a caller tracking unsaved changes must
    /// treat the database as modified.
    pub fn mutation_count(&self) -> u64 {
        self.mutations
    }

    /// The per-predicate CSR of the **default graph** (named graphs are not
    /// part of it — upstream has none) that gStore's advanced graph functions
    /// run on; see [`CsrGraph`].
    ///
    /// Built on first use and cached; the cache is keyed by
    /// [`mutation_count`](Self::mutation_count), so the next call after any
    /// applied insert, delete, `CLEAR`/`DROP`, `LOAD`, inference or rollback
    /// rebuilds it (upstream rebuilds in `prepPathQuery` when the KVstore's
    /// CSR-dirty flag is set, GeneralEvaluation.cpp:262-271). A named-graph
    /// mutation also advances the counter and so triggers a (redundant but
    /// harmless) rebuild. Thread-safe: concurrent callers share one build.
    pub fn csr(&self) -> Result<std::sync::Arc<CsrGraph>> {
        self.csr_cache
            .get_or_build(self.mutations, || CsrGraph::build(&self.store))
    }

    /// Build the CSR now instead of on the first graph-function query —
    /// upstream's `loadcsr` operation and `load` with `csr=1` / `gserver -c 1`.
    pub fn preload_csr(&self) -> Result<()> {
        self.csr().map(|_| ())
    }

    /// Whether a CSR for the current data is cached (no rebuild needed).
    pub fn csr_is_cached(&self) -> bool {
        self.csr_cache.peek(self.mutations).is_some()
    }

    /// A view of [`csr_is_cached`](Self::csr_is_cached) that can be read
    /// without access to the database — so without waiting for whoever
    /// holds it (a server's `show` reads it while a statement runs). A write
    /// clears it; it reads `false` once the database is dropped.
    pub fn csr_status(&self) -> crate::analytics::csr::CsrStatus {
        self.csr_cache.status()
    }

    /// The cached CSR if it is current for the data (never builds) — lets a
    /// read snapshot of this database share it instead of rebuilding.
    pub fn cached_csr(&self) -> Option<std::sync::Arc<CsrGraph>> {
        self.csr_cache.peek(self.mutations)
    }

    /// Record an applied data mutation: invalidate cached query results,
    /// drop the now stale CSR (so it is not kept alive until the next graph
    /// function query) and advance [`mutation_count`](Self::mutation_count).
    fn touched(&mut self) {
        self.query_cache.get_mut().clear();
        self.csr_cache.clear();
        self.mutations += 1;
    }

    /// Rebuild the VS-tree from the current store and mark the index valid.
    pub fn rebuild_index(&mut self) {
        self.vstree = build_vstree(&self.store);
        self.index_valid = true;
    }

    /// Opt this database into an **out-of-core** (streaming) VS-tree, even while
    /// the triple store itself stays in memory. The signature index is written
    /// to a paged node file at `path`, and every candidate filter then streams
    /// in only the nodes a query traverses — so a large index need not stay
    /// resident in RAM. Only the VS-tree becomes out-of-core; the store is
    /// untouched.
    ///
    /// The in-memory tree remains the default; call this to opt in. Query
    /// results are unchanged: the streaming filter returns candidate sets
    /// identical to the in-memory tree (both satisfy
    /// [`crate::signature::SigFilter`]), so this is purely a residency/space
    /// trade-off. The rebuilt index is left valid; a later mutation invalidates
    /// it (an out-of-core tree can't be updated in place — see
    /// [`insert_triple`](Self::insert_triple)), and
    /// [`rebuild_index`](Self::rebuild_index) restores an in-memory one.
    pub fn use_streaming_vstree<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        let path = path.as_ref();
        // Ensure the parent directory exists (the pager creates the file, not
        // the directory tree above it).
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        // Fresh build: drop any prior node file (and its WAL) at this path.
        remove_file_and_wal(path);
        self.vstree = VsTree::build_disk(path, vstree_entries(&self.store))?;
        self.index_valid = true;
        self.query_cache.get_mut().clear();
        Ok(())
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

    /// The user-defined functions registered on this database (a clone is
    /// cheap and shares the closures). Read snapshots of the database hand
    /// them to their evaluators, so a query sees the same functions there as
    /// through [`query`](Self::query).
    pub fn functions(&self) -> &FunctionRegistry {
        &self.functions
    }

    /// Set which sources SPARQL `LOAD` may read. The default,
    /// [`LoadPolicy::permissive`], reads any local path (CLI behaviour); a
    /// server exposing updates to clients should install
    /// [`LoadPolicy::restricted`] (optionally re-allowing an upload directory
    /// with [`LoadPolicy::with_allowed_dir`]). Not persisted with the database.
    pub fn set_load_policy(&mut self, policy: LoadPolicy) {
        self.load_policy = policy;
    }

    /// The active `LOAD` policy.
    pub fn load_policy(&self) -> &LoadPolicy {
        &self.load_policy
    }

    /// Build a database by importing one or more RDF files. The format of each
    /// file is detected by extension (N-Triples/Turtle/N-Quads/TriG/RDF-XML/
    /// JSON-LD/RDFa; unknown → Turtle) via [`parser::parse_file_by_ext`].
    pub fn build_from_files<P: AsRef<Path>>(
        name: impl Into<String>,
        files: &[P],
    ) -> Result<Database> {
        let mut db = Database::new(name);
        let mut id_triples: Vec<IdTriple> = Vec::new();
        for f in files {
            for t in crate::parser::parse_file_by_ext(f)? {
                id_triples.push(db.encode_triple(&t)?);
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
            id_triples.push(db.encode_triple(t)?);
        }
        db.store.bulk_load(id_triples);
        progress(BuildProgress::Index);
        db.rebuild_index();
        progress(BuildProgress::Done);
        Ok(db)
    }

    // (RocksDB / on-disk / snapshot persistence lives in `persist.rs`.)

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
    /// [`Backend::Memory`] backend — which an on-disk database also uses for its
    /// working set (used by snapshot MVCC and tests); panics for a
    /// RocksDB-backed database, which has no in-RAM store — query it through
    /// [`query`](Self::query)/[`select`](Self::select) or use
    /// [`backend`](Self::backend) instead.
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

    /// Encode a triple to ids, interning any new terms. Errs (interning
    /// nothing further) once a dictionary id space is exhausted.
    fn encode_triple(&mut self, t: &Triple) -> Result<IdTriple> {
        let sub = self.dict.try_intern_entity(&t.subject.dict_key())?;
        let pred = self.dict.try_intern_predicate(&t.predicate.dict_key())?;
        let obj = self.dict.try_intern_term(&t.object)?;
        Ok(IdTriple::new(sub, pred, obj))
    }

    /// Insert one triple. Returns `true` if it was newly added; `false` if it
    /// was already present **or** could not be encoded because a dictionary id
    /// space is exhausted — use [`try_insert_triple`](Self::try_insert_triple)
    /// to tell those apart. On a persistent layout the insert is also written
    /// through to disk (see [`StorageKind`]); a persistence failure is reported
    /// by the next commit point (an auto-commit UPDATE,
    /// [`commit`](Self::commit), [`save`](Self::save)).
    pub fn insert_triple(&mut self, t: &Triple) -> bool {
        self.try_insert_triple(t).unwrap_or(false)
    }

    /// Insert one triple, reporting id-space exhaustion as an error. `Ok(true)`
    /// if it was newly added, `Ok(false)` if it was already present (or a
    /// persistence write failed; that failure is reported by the next commit
    /// point).
    pub fn try_insert_triple(&mut self, t: &Triple) -> Result<bool> {
        let Some((_, id)) = self.intern_for_insert(t, None)? else {
            return Ok(false);
        };
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
            self.touched();
            if let Some(log) = self.txn.as_mut() {
                log.push(UndoOp::Del(None, id)); // rollback removes what we added
            }
            self.record_redo(b'I', None, id);
            self.persist_mutation(true, None, id);
        }
        Ok(changed)
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
            self.touched();
            if let Some(log) = self.txn.as_mut() {
                log.push(UndoOp::Add(None, id)); // rollback re-adds what we removed
            }
            self.record_redo(b'D', None, id);
            self.persist_mutation(false, None, id);
        }
        changed
    }

    // ---- transactions -----------------------------------------------------

    /// Begin a transaction (single-writer). Triple mutations are recorded so
    /// they can be undone with [`rollback`](Self::rollback); [`commit`](Self::commit)
    /// makes them permanent. Returns an error if a transaction is already active
    /// (nesting is not supported). Provides atomicity + rollback; full MVCC
    /// (version chains, locking, GC) is a separate concern (see REFACTOR_BACKLOG).
    ///
    /// On a RocksDB database the transaction's writes are journaled so that it
    /// stays atomic across a crash: uncommitted writes are undone on the next
    /// open (see [`crate::backend::rocks`]).
    pub fn begin(&mut self) -> Result<()> {
        if self.txn.is_some() {
            return Err(GStoreError::Database(
                "a transaction is already active".into(),
            ));
        }
        self.rocks_begin_journal()?;
        self.txn = Some(Vec::new());
        Ok(())
    }

    /// Commit the active transaction, discarding its undo log. Buffered redo
    /// records (this transaction's mutations) are flushed to the redo log, since
    /// the transaction is now durable.
    ///
    /// On a persistent layout this is a commit point: the transaction's queued
    /// write-through reaches the store and is made durable (see [`StorageKind`]).
    pub fn commit(&mut self) -> Result<()> {
        if self.txn.take().is_none() {
            return Err(GStoreError::Database("no active transaction".into()));
        }
        self.disk_commit_pending();
        let redo = self.flush_redo();
        // RocksDB: deleting the undo journal is what commits the transaction.
        let journal = self.rocks_commit_journal();
        let point = self.commit_point();
        redo.and(journal).and(point)
    }

    /// Roll back the active transaction, undoing every triple mutation made
    /// since [`begin`](Self::begin) in reverse order.
    pub fn rollback(&mut self) -> Result<()> {
        let Some(log) = self.txn.take() else {
            return Err(GStoreError::Database("no active transaction".into()));
        };
        // The transaction's mutations are being undone, so its buffered redo
        // records must never reach the log, nor its queued write-through the
        // on-disk store (which never saw them).
        self.redo_pending.clear();
        self.disk_discard_pending();
        // A RocksDB store is rolled back by replaying its undo journal (which
        // also covers the named-graph quads in RocksDB); only the in-memory
        // named-graph map is undone here.
        let journaled = self.rocks_journaling();
        for op in log.into_iter().rev() {
            match op {
                UndoOp::Add(None, id) => {
                    if !journaled {
                        self.store.insert(id);
                    }
                }
                UndoOp::Del(None, id) => {
                    if !journaled {
                        self.store.remove(id);
                    }
                }
                UndoOp::Add(Some(gid), id) => {
                    if self.named.entry(gid).or_default().insert(id) && !journaled {
                        self.persist_named_quad(true, gid, id);
                    }
                }
                UndoOp::Del(Some(gid), id) => {
                    if self.named.get_mut(&gid).is_some_and(|s| s.remove(id)) && !journaled {
                        self.persist_named_quad(false, gid, id);
                    }
                }
            }
        }
        let replayed = self.rocks_rollback_journal();
        self.index_valid = false;
        self.touched();
        replayed
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
    fn insert_quad(&mut self, t: &Triple, graph: Option<&str>) -> Result<bool> {
        let Some(g) = graph else {
            return self.try_insert_triple(t);
        };
        let Some((Some(gid), id)) = self.intern_for_insert(t, Some(g))? else {
            return Ok(false);
        };
        let present = self
            .named
            .get(&gid)
            .is_some_and(|s| s.exists(id.sub, id.pred, id.obj));
        // RocksDB first: if the quad cannot be persisted it is not applied at
        // all (so a retry writes it again), and the error is reported at the
        // next commit point.
        if present || !self.persist_named_quad(true, gid, id) {
            return Ok(false);
        }
        self.named.entry(gid).or_default().insert(id);
        self.touched();
        if let Some(log) = self.txn.as_mut() {
            log.push(UndoOp::Del(Some(gid), id));
        }
        self.record_redo(b'I', Some(gid), id);
        self.persist_mutation(true, Some(gid), id);
        Ok(true)
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
        let present = self
            .named
            .get(&gid)
            .is_some_and(|s| s.exists(id.sub, id.pred, id.obj));
        // RocksDB first (see `insert_quad`).
        if !present || !self.persist_named_quad(false, gid, id) {
            return false;
        }
        if let Some(s) = self.named.get_mut(&gid) {
            s.remove(id);
        }
        self.touched();
        if let Some(log) = self.txn.as_mut() {
            log.push(UndoOp::Add(Some(gid), id));
        }
        self.record_redo(b'D', Some(gid), id);
        self.persist_mutation(false, Some(gid), id);
        true
    }

    /// Apply `INSERT DATA`. Returns the number of triples newly added. A triple
    /// that cannot be encoded (dictionary id space exhausted) is skipped; see
    /// [`try_insert_data`](Self::try_insert_data) to surface that as an error.
    pub fn insert_data(&mut self, triples: &[GroundTriple]) -> usize {
        triples
            .iter()
            .filter(|g| {
                self.insert_quad(&Self::ground_to_triple(g), g.graph.as_deref())
                    .unwrap_or(false)
            })
            .count()
    }

    /// Apply `INSERT DATA`, stopping with an error if a dictionary id space is
    /// exhausted (triples before the failing one stay inserted, as with any
    /// mid-update failure; wrap in a transaction to make it atomic).
    pub fn try_insert_data(&mut self, triples: &[GroundTriple]) -> Result<usize> {
        let mut n = 0;
        for g in triples {
            if self.insert_quad(&Self::ground_to_triple(g), g.graph.as_deref())? {
                n += 1;
            }
        }
        Ok(n)
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
        let added = self.infer(|dict, store, insert| {
            crate::reason::materialize_with(dict, store, insert)
        });
        self.record_inferred(added)
    }

    /// Book-keeping after reasoning inserted `added` into the default graph:
    /// invalidate the index and cache, log them for rollback, and write them
    /// through (on-disk). Returns how many there were.
    fn record_inferred(&mut self, added: Vec<IdTriple>) -> usize {
        let n = added.len();
        if n > 0 {
            self.index_valid = false;
            self.touched();
            // Record inferred triples so reasoning inside a transaction rolls back.
            if let Some(log) = self.txn.as_mut() {
                for t in &added {
                    log.push(UndoOp::Del(None, *t));
                }
            }
            self.persist_inferred(&added);
        }
        n
    }

    // ---- query ------------------------------------------------------------

    /// Parse and run a SPARQL request. SELECT/ASK read; INSERT/DELETE DATA write.
    pub fn query(&mut self, sparql: &str) -> Result<QueryResult> {
        self.query_inner(sparql, None, None)
    }

    /// Like [`query`](Self::query) but cooperatively cancellable: when `cancel`
    /// flips to `true` (from another thread), an in-flight read aborts at the next
    /// pattern/phase boundary with [`GStoreError::Cancelled`]. So does the
    /// WHERE clause of an UPDATE's `DELETE`/`INSERT … WHERE` (which may call a
    /// graph function in a sub-SELECT): it is evaluated before anything is
    /// applied, so a cancelled operation changes nothing — operations of the
    /// same request before it stay applied, as after any failure. Mirrors
    /// gStore's task cancellation.
    ///
    /// The multi-database [`ApiServer`](crate::http_api::ApiServer) runs each
    /// connection on its own worker thread and executes queries without its
    /// state lock held, so `POST /cancel` (and a graceful shutdown past its grace
    /// period) reaches a running query through this flag.
    pub fn query_cancellable(
        &mut self,
        sparql: &str,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<QueryResult> {
        self.query_inner(sparql, Some(cancel), None)
    }

    /// [`query`](Self::query) / [`query_cancellable`](Self::query_cancellable)
    /// on behalf of `pfn`: `PFN.name({…})` calls reach `pfn`'s functions (a
    /// server passes the authenticated user, see [`crate::pfn::PfnCaller`])
    /// instead of the [installed caller](Self::set_pfn_caller), under this
    /// database's PFN limits.
    pub fn query_as(
        &mut self,
        sparql: &str,
        cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        pfn: Option<&crate::pfn::PfnCaller>,
    ) -> Result<QueryResult> {
        self.query_inner(sparql, cancel, pfn)
    }

    fn query_inner(
        &mut self,
        sparql: &str,
        cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        pfn: Option<&crate::pfn::PfnCaller>,
    ) -> Result<QueryResult> {
        let q = sparql::parse(sparql)?;
        match q {
            Query::Select(_) | Query::Ask(_) | Query::Construct(_) | Query::Describe(_) => {
                // Serve from the query cache when the same request was already
                // answered against the current (unchanged) store. Only
                // deterministic queries are cached (no NOW()/RAND()/UUID()/
                // STRUUID()/BNODE(), SERVICE or user functions).
                let cacheable = query_cache::is_cacheable(&q);
                if cacheable {
                    if let Some(cached) = self.query_cache.borrow_mut().get(sparql, self.mutations)
                    {
                        return Ok(cached);
                    }
                }
                // The advanced graph functions run on this database's cached
                // CSR (see `csr`), built only when a query needs it.
                let csr = || self.csr();
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
                // gStore's PFN functions: the caller's, under this database's
                // limits (never cached, see `query_cache::is_cacheable`).
                if let Some(caller) = pfn.or(self.pfn.as_ref()) {
                    eval = eval.with_pfn(caller.clone().with_limits(self.pfn_limits));
                }
                if let Some(c) = cancel {
                    eval = eval.with_cancel(c);
                }
                // `SERVICE` makes outbound requests: only where remote sources
                // are allowed (not on a server's restricted policy).
                eval = eval
                    .with_service_allowed(self.load_policy.allow_remote)
                    .with_csr_provider(&csr)
                    .with_integer_division(self.integer_division)
                    .with_topk_options(self.topk)
                    .with_gfun_max_output_bytes(self.gfun_max_output_bytes);
                let result = eval.evaluate(&q)?;
                if cacheable {
                    self.query_cache.borrow_mut().put(
                        sparql.to_string(),
                        result.clone(),
                        self.mutations,
                    );
                }
                Ok(result)
            }
            Query::Update(ops) => self.run_update(sparql, ops, cancel),
        }
    }

    /// Apply the parsed operations `ops` of UPDATE request `sparql` (the
    /// text the update log records): the write path of
    /// [`query`](Self::query), shared with the reasoning rules
    /// ([`reason_execute`](Self::reason_execute) /
    /// [`reason_disable`](Self::reason_disable)).
    fn run_update(
        &mut self,
        sparql: &str,
        ops: Vec<UpdateOp>,
        cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<QueryResult> {
        // Reject a statically invalid request (an unknown IRI
        // function, a graph function in FILTER/BIND, …) before any of
        // its operations is applied.
        if ops.iter().any(|op| matches!(op, UpdateOp::Modify { .. })) {
            let mut eval = Evaluator::new(&self.dict, &self.store);
            if !self.functions.is_empty() {
                eval = eval.with_functions(self.functions.clone());
            }
            eval.check_update_calls(&ops)?;
        }
        let before = self.mutations;
        let mut changed = 0;
        for op in ops {
            match self.exec_update_op(op, cancel.as_ref()) {
                Ok(n) => changed += n,
                Err(e) => return Err(self.failed_update(e, before)),
            }
        }
        // Append the applied statement to the update log, if enabled.
        if let Some(path) = self.update_log.clone() {
            append_update_log(&path, sparql, changed)?;
        }
        // Outside a transaction each UPDATE request is a commit point:
        // its write-through is made durable before it is acknowledged.
        if self.txn.is_none() {
            self.commit_point()?;
        }
        Ok(QueryResult::Update { changed })
    }

    /// An UPDATE failed with `e` after its earlier operations (or part of the
    /// failing one) may already have been applied — they stay applied, as
    /// documented on [`try_insert_data`](Self::try_insert_data). Outside a
    /// transaction the applied part is still made durable (this is the
    /// request's commit point), so the persistent store never lags what
    /// readers see; `before` is the [`mutation_count`](Self::mutation_count)
    /// the update started at.
    fn failed_update(&mut self, e: GStoreError, before: u64) -> GStoreError {
        if self.txn.is_some() || self.mutations == before {
            return e;
        }
        match self.commit_point() {
            Ok(()) => e,
            Err(pe) => GStoreError::Database(format!(
                "{e}; in addition, the part of the update applied before the failure could not \
                 be persisted: {pe}"
            )),
        }
    }

    /// Apply one UPDATE operation, returning the number of triples it changed.
    /// A `DELETE`/`INSERT … WHERE` evaluates its WHERE clause under `cancel`.
    fn exec_update_op(
        &mut self,
        op: UpdateOp,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<usize> {
        match op {
            UpdateOp::InsertData(triples) => self.try_insert_data(&triples),
            UpdateOp::DeleteData(triples) => Ok(self.delete_data(&triples)),
            UpdateOp::Modify {
                with,
                delete,
                insert,
                using,
                using_named,
                pattern,
            } => {
                // Compute the ground delete/insert quads against the *current*
                // data, then apply deletes before inserts (SPARQL semantics).
                // `modify_changes` evaluates the WHERE clause under this
                // database's `SERVICE` policy and the request's cancel flag:
                // a refused `SERVICE` or a cancel fails the update before
                // anything is applied.
                let (dels, ins) = self.modify_changes(
                    &delete,
                    &insert,
                    &pattern,
                    with.as_deref(),
                    &using,
                    &using_named,
                    cancel,
                )?;
                // A template triple outside `GRAPH { … }` targets the WITH
                // graph, else the default graph.
                let target = |g: &Option<String>| g.clone().or_else(|| with.clone());
                let mut changed = 0;
                for (g, t) in &dels {
                    if self.delete_quad(t, target(g).as_deref()) {
                        changed += 1;
                    }
                }
                for (g, t) in &ins {
                    if self.insert_quad(t, target(g).as_deref())? {
                        changed += 1;
                    }
                }
                Ok(changed)
            }
            UpdateOp::Load {
                source,
                silent,
                into,
            } => match self.load_rdf_source(&source, into.as_deref()) {
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
            UpdateOp::Add { from, to, silent } => {
                self.transfer_graph(GraphTransfer::Add, from, to, silent)
            }
            UpdateOp::Move { from, to, silent } => {
                self.transfer_graph(GraphTransfer::Move, from, to, silent)
            }
            UpdateOp::Copy { from, to, silent } => {
                self.transfer_graph(GraphTransfer::Copy, from, to, silent)
            }
        }
    }

    /// `ADD` / `MOVE` / `COPY [SILENT] from TO to` (SPARQL 1.1 Update
    /// §3.2.3-3.2.5; a graph is `None` for `DEFAULT`). Returns the number of
    /// triples changed: those cleared from the destination (COPY/MOVE), added
    /// to it, and dropped from the source (MOVE).
    ///
    /// * The same graph on both sides is a no-op, whatever the operation.
    /// * The destination is created as needed.
    /// * A named source graph that holds no triples does not exist — this
    ///   store keeps no empty named graphs (CREATE GRAPH declares nothing) —
    ///   and the operation fails, as the spec allows, unless SILENT, which
    ///   makes it a successful no-op. The default graph always exists.
    fn transfer_graph(
        &mut self,
        op: GraphTransfer,
        from: Option<String>,
        to: Option<String>,
        silent: bool,
    ) -> Result<usize> {
        if from == to {
            return Ok(0);
        }
        let Some(triples) = self.graph_triples(from.as_deref()) else {
            if silent {
                return Ok(0);
            }
            return Err(GStoreError::Query(format!(
                "{} failed: the source graph <{}> does not exist (it holds no triples); use {} \
                 SILENT to ignore a missing source",
                op.keyword(),
                from.as_deref().unwrap_or_default(),
                op.keyword()
            )));
        };
        let mut changed = 0;
        if matches!(op, GraphTransfer::Copy | GraphTransfer::Move) {
            changed += match to.as_deref() {
                None => self.clear_default(),
                Some(g) => self.clear_named(g),
            };
        }
        for t in &triples {
            if self.insert_quad(t, to.as_deref())? {
                changed += 1;
            }
        }
        if op == GraphTransfer::Move {
            changed += match from.as_deref() {
                None => self.clear_default(),
                Some(g) => self.clear_named(g),
            };
        }
        Ok(changed)
    }

    /// Every triple of the default graph (`None`) or of a named graph, as
    /// terms; `None` for a named graph that holds no triples.
    fn graph_triples(&self, graph: Option<&str>) -> Option<Vec<Triple>> {
        let ids = match graph {
            None => self.store.iter_all(),
            Some(g) => {
                let gid = self.dict.entity_id(&Term::iri(g).dict_key())?;
                let ids: Vec<IdTriple> = self.named.get(&gid)?.iter_all().collect();
                if ids.is_empty() {
                    return None;
                }
                ids
            }
        };
        let term = |s: Option<&str>| s.and_then(|s| crate::parser::ntriples::parse_term(s).ok());
        Some(
            ids.into_iter()
                .filter_map(|id| {
                    Some(Triple::new(
                        term(self.dict.id_to_string(id.sub))?,
                        term(self.dict.predicate_to_string(id.pred))?,
                        term(self.dict.id_to_string(id.obj))?,
                    ))
                })
                .collect(),
        )
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
                // An on-disk database must still delete them from its store.
                if self.is_disk_backed() {
                    for &t in &removed {
                        self.persist_mutation(false, None, t);
                    }
                }
            } else {
                // Persistent backend: delete every triple through the seam so the
                // change is durable (don't swap in a Memory store, which would
                // strand the data on disk).
                for t in removed {
                    self.store.remove(t);
                }
            }
            self.index_valid = false;
            self.touched();
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
            if let Some(removed) = self.named.remove(&gid) {
                self.persist_cleared_graph(gid, removed);
            }
            self.touched();
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
            for (gid, removed) in std::mem::take(&mut self.named) {
                self.persist_cleared_graph(gid, removed);
            }
            self.touched();
        }
        total
    }

    /// Persist the removal of named graph `gid` (already taken out of
    /// [`named`](Self::named)) whose quads were `removed`. A quad RocksDB could
    /// not delete is put back into the in-memory graph, so memory never claims
    /// a deletion the store did not make (the error is reported at the next
    /// commit point).
    fn persist_cleared_graph(&mut self, gid: u32, removed: TripleStore) {
        let mut kept = TripleStore::new();
        for t in removed.iter_all() {
            self.persist_mutation(false, Some(gid), t);
            if !self.persist_named_quad(false, gid, t) {
                kept.insert(t);
            }
        }
        if kept.triple_count() > 0 {
            self.named.insert(gid, kept);
        }
    }

    /// The ground quads a `DELETE … INSERT … WHERE` removes and adds (see
    /// [`Evaluator::eval_update_modify`]; `None` = the operation's default
    /// graph). The WHERE clause matches against the request's dataset
    /// (SPARQL 1.1 Update §3.1.3):
    ///
    /// * with `USING`/`USING NAMED` clauses: the default graph is the merge
    ///   of the `USING` graphs (empty when there are none) and the named
    ///   graphs are exactly the `USING NAMED` ones;
    /// * otherwise with `WITH <g>`: the default graph is `<g>` (empty if it
    ///   does not exist) and every named graph is visible to `GRAPH`;
    /// * otherwise the store's default graph and all named graphs.
    ///
    /// The WHERE clause runs under `cancel` (the request's cancel flag), as
    /// a read query does.
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    fn modify_changes(
        &self,
        delete: &[QuadPattern],
        insert: &[QuadPattern],
        pattern: &GraphPattern,
        with: Option<&str>,
        using: &[String],
        using_named: &[String],
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<(Vec<(Option<String>, Triple)>, Vec<(Option<String>, Triple)>)> {
        let graph = |iri: &str| {
            self.dict
                .entity_id(&Term::iri(iri).dict_key())
                .and_then(|gid| self.named.get(&gid).map(|s| (gid, s)))
        };
        fn prepare<'e, S: TripleSource>(
            db: &Database,
            eval: Evaluator<'e, S>,
            cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
        ) -> Evaluator<'e, S> {
            // IRI function calls in the WHERE clause reach the user-defined
            // functions, as in read queries; `SERVICE` follows the load
            // policy (refused on a server's restricted policy), as in reads;
            // and the request's cancel flag stops it (a graph function in a
            // sub-SELECT included), as in reads.
            let mut eval = eval
                .with_service_allowed(db.load_policy.allow_remote)
                .with_integer_division(db.integer_division)
                .with_topk_options(db.topk)
                .with_gfun_max_output_bytes(db.gfun_max_output_bytes);
            if let Some(c) = cancel {
                eval = eval.with_cancel(std::sync::Arc::clone(c));
            }
            if let Some(caller) = db.pfn_caller() {
                eval = eval.with_pfn(caller);
            }
            if db.functions.is_empty() {
                eval
            } else {
                eval.with_functions(db.functions.clone())
            }
        }
        if !using.is_empty() || !using_named.is_empty() {
            let mut merged = TripleStore::new();
            for iri in using {
                if let Some((_, s)) = graph(iri) {
                    for t in s.iter_all() {
                        merged.insert(t);
                    }
                }
            }
            let named: BTreeMap<u32, TripleStore> = using_named
                .iter()
                .filter_map(|iri| graph(iri).map(|(gid, s)| (gid, s.clone())))
                .collect();
            let eval =
                prepare(self, Evaluator::new(&self.dict, &merged), cancel).with_named(&named);
            return eval.eval_update_modify(delete, insert, pattern);
        }
        if let Some(w) = with {
            let empty = TripleStore::new();
            let store = graph(w).map_or(&empty, |(_, s)| s);
            let eval =
                prepare(self, Evaluator::new(&self.dict, store), cancel).with_named(&self.named);
            return eval.eval_update_modify(delete, insert, pattern);
        }
        let csr = || self.csr();
        let eval = prepare(self, Evaluator::new(&self.dict, &self.store), cancel)
            .with_named(&self.named)
            .with_csr_provider(&csr);
        eval.eval_update_modify(delete, insert, pattern)
    }

    /// `LOAD <iri> [INTO GRAPH <g>]`: read an RDF (Turtle/N-Triples) document
    /// into the default graph, or into named graph `into`, subject to the
    /// [`LoadPolicy`]. Only local sources are fetched — a `file:` IRI or a
    /// bare path; remote (`http(s)://`, …) sources are never retrieved (no
    /// network) and error.
    fn load_rdf_source(&mut self, source: &str, into: Option<&str>) -> Result<usize> {
        let path = match self.load_policy.resolve(source)? {
            LoadSource::Local(path) => path,
            LoadSource::Remote(_) => {
                return Err(GStoreError::Query(format!(
                    "LOAD of remote source '{source}' is not supported (no network); \
                     use a local file path or file:// IRI"
                )))
            }
        };
        let triples = turtle::parse_file(&path)?;
        let mut n = 0;
        for t in &triples {
            if self.insert_quad(t, into)? {
                n += 1;
            }
        }
        Ok(n)
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

    // ---- backup / restore -------------------------------------------------

    /// Back up a consistent **full** snapshot of this database into `backup_dir`
    /// (gStore `Database::backup`). The snapshot is fully independent of the live
    /// database; restore it with [`restore`](Self::restore).
    ///
    /// This also establishes the *incremental-backup baseline*: it records the
    /// current **redo-log** length as the checkpoint (and writes a small
    /// `BACKUP_OFFSET` marker into `backup_dir`), so a later
    /// [`backup_incremental`](Self::backup_incremental) captures only the triple
    /// mutations committed after this point. The full snapshot already reflects
    /// every mutation up to the checkpoint, so base + subsequent increments
    /// reconstruct the latest committed state without overlap or gap.
    ///
    /// (Incremental backup is redo-log based — it captures *all* committed triple
    /// mutations, including direct-API and [`ConcurrentDb`](crate::concurrent::ConcurrentDb)
    /// transactions, and rolled-back transactions are excluded — so enable redo
    /// logging, not the SPARQL-statement update log, if you intend to take
    /// increments. See [`enable_redo_log_in`](Self::enable_redo_log_in).)
    pub fn backup<P: AsRef<Path>>(&self, backup_dir: P) -> Result<()> {
        let backup_dir = backup_dir.as_ref();
        self.save(backup_dir)?;
        self.backup_reason_rules(backup_dir)?;
        let off = self.redo_log_len();
        self.backup_offset.set(off);
        write_backup_offset(&backup_dir.join(BACKUP_OFFSET_FILE), off)?;
        Ok(())
    }

    /// Restore a database from a full backup directory created by
    /// [`backup`](Self::backup) (gStore `Database::restore`), whatever its layout.
    ///
    /// A snapshot backup is loaded into memory (the backup directory is never
    /// written). An on-disk or RocksDB backup is *opened in place*, so later
    /// updates would be written into the backup itself — to restore one into a
    /// database directory, use [`restore_into`](Self::restore_into) and open the
    /// target instead.
    pub fn restore<P: AsRef<Path>>(backup_dir: P) -> Result<Database> {
        Database::open(backup_dir)
    }

    /// The current incremental-backup checkpoint: the redo-log byte offset that
    /// the most recent full/incremental backup ended at (0 before any backup).
    pub fn backup_checkpoint(&self) -> u64 {
        self.backup_offset.get()
    }

    /// Byte length of the redo log, or 0 when logging is disabled or the file
    /// does not exist yet. This is the incremental-backup checkpoint unit: the
    /// redo log is append-only and every record is written whole, so its length
    /// is always a clean record boundary.
    fn redo_log_len(&self) -> u64 {
        self.redo_log
            .as_ref()
            .and_then(|p| fs::metadata(p).ok())
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// Produce an **incremental** backup into `out_dir`: the delta relative to the
    /// most recent full ([`backup`](Self::backup)) or incremental backup
    /// (C++ gStore's full + incremental backup pairing). It copies the **redo-log**
    /// records appended since the last checkpoint into `out_dir/incr.log` and
    /// writes an `INCREMENTAL` marker recording the `[base_offset, end_offset)`
    /// byte range it covers, then advances the checkpoint — so successive
    /// incrementals chain contiguously.
    ///
    /// Requires **redo** logging to be enabled (`enable_redo_log*`). The delta is
    /// the concrete triple mutations (insert/delete, default or named graph)
    /// committed since the base — so it faithfully captures *every* committed
    /// mutation regardless of its source (SPARQL UPDATE, direct-API
    /// insert/remove, and [`ConcurrentDb`](crate::concurrent::ConcurrentDb)
    /// transactions), excludes rolled-back transactions (their redo records are
    /// discarded), and replays deterministically (no re-reading of external `LOAD`
    /// sources). Reconstruct the latest committed state with
    /// [`restore_incremental`](Self::restore_incremental).
    ///
    /// Format of `out_dir`:
    /// * `incr.log` — the raw bytes of the redo log for `[base, end)`, in the same
    ///   length-prefixed `REDO <op> <graph_len> <triple_len>\n…` record format, so
    ///   it replays through the ordinary redo-log machinery.
    /// * `INCREMENTAL` — a UTF-8 marker: `GSTORE-INCREMENTAL 1\nbase_offset <B>\nend_offset <E>\n`.
    pub fn backup_incremental<P: AsRef<Path>>(&self, out_dir: P) -> Result<()> {
        let out_dir = out_dir.as_ref();
        let Some(log_path) = self.redo_log.clone() else {
            return Err(GStoreError::Database(
                "backup_incremental requires redo logging to be enabled \
                 (call enable_redo_log / enable_redo_log_in first)"
                    .into(),
            ));
        };
        let base = self.backup_offset.get();
        // Missing log ⇒ no records yet (treat as empty), so an incremental taken
        // before any logged mutation is simply an empty (but valid) delta.
        let data = fs::read(&log_path).unwrap_or_default();
        let end = data.len() as u64;
        if end < base {
            return Err(GStoreError::Database(format!(
                "redo log ({end} bytes) shrank below the backup checkpoint ({base}); \
                 the log was truncated or replaced since the last backup"
            )));
        }
        fs::create_dir_all(out_dir)?;
        let slice = &data[base as usize..end as usize];
        fsutil::atomic_write(&out_dir.join(INCREMENTAL_LOG_FILE), slice)?;
        write_incremental_marker(&out_dir.join(INCREMENTAL_MARKER_FILE), base, end)?;
        self.backup_offset.set(end);
        Ok(())
    }

    /// Restore from a full base backup and then apply incremental deltas in
    /// order, reconstructing the state captured by the last incremental
    /// (C++ gStore's incremental-restore path).
    ///
    /// `base_dir` is a directory produced by [`backup`](Self::backup); each entry
    /// of `incr_dirs` is a directory produced by
    /// [`backup_incremental`](Self::backup_incremental), in chronological order.
    /// The base (any layout) is loaded and detached into memory — the backup
    /// directories are never modified — then each delta's `incr.log` is replayed
    /// onto it. Deltas are checked for contiguity: each must begin exactly where
    /// the previous one ended (starting from the base's recorded checkpoint), so
    /// an out-of-order or gapped chain is rejected rather than silently
    /// misapplied. Persist the result with [`save`](Self::save).
    pub fn restore_incremental<P: AsRef<Path>, Q: AsRef<Path>>(
        base_dir: P,
        incr_dirs: &[Q],
    ) -> Result<Database> {
        let base_dir = base_dir.as_ref();
        let mut db = Database::open(base_dir)?.into_memory()?;
        // The base's checkpoint is where the first increment must begin. A missing
        // marker (a backup taken without logging) legitimately means offset 0, but
        // a marker that exists yet cannot be read is a corrupt base — surface that
        // rather than silently pretending the checkpoint was 0.
        let marker = base_dir.join(BACKUP_OFFSET_FILE);
        let mut expected = if marker.exists() {
            read_backup_offset(&marker)?
        } else {
            0
        };
        // Replaying must not re-log; a freshly loaded db has no redo log anyway.
        let saved = db.redo_log.take();
        let outcome = (|| {
            for dir in incr_dirs {
                let dir = dir.as_ref();
                let (base, end) =
                    read_incremental_marker(&dir.join(INCREMENTAL_MARKER_FILE))?;
                if base != expected {
                    return Err(GStoreError::Database(format!(
                        "incremental backup '{}' starts at offset {base} but {expected} \
                         was expected (out-of-order or gapped increment chain)",
                        dir.display()
                    )));
                }
                let bytes = fs::read(dir.join(INCREMENTAL_LOG_FILE))?;
                db.apply_redo_records(&bytes)?;
                expected = end;
            }
            Ok(())
        })();
        db.redo_log = saved;
        db.backup_offset.set(expected);
        outcome.map(|()| db)
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
                        self.insert_quad(&t, graph_opt)?;
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

        let freed = self.dict.reclaim_unused(&ent_lit, &preds);
        if freed > 0 {
            self.persist_dictionary_rewrite();
        }
        freed
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
        let mut rules = std::mem::take(&mut self.rules);
        let added = self.infer(|dict, store, insert| rules.apply_with(dict, store, insert));
        self.rules = rules;
        self.record_inferred(added)
    }

    // ---- upstream reasoning rules (`reasonManage`) --------------------------
    //
    // The lifecycle of upstream gStore's JSON rules — add, list, compile,
    // execute, disable, show, delete, check — implemented in
    // `crate::reason::manage` (see its docs for the states, the `Rule:`
    // predicates and the ledger that makes retraction exact). Rules live in
    // `<dir>/reason_rule_files/` for a database with a directory, else in
    // memory (see `reason_db_dir`).

    /// The directory whose `reason_rule_files/` holds this database's
    /// upstream-format reasoning rules: the one set with
    /// [`set_reason_db_dir`](Self::set_reason_db_dir), else the database's
    /// [`home`](Self::home). `None`: the rules are kept in memory and live as
    /// long as this value — [`backup`](Self::backup) writes them into the
    /// backup, [`save`](Self::save) does not (open the saved directory, or
    /// [`set_reason_db_dir`](Self::set_reason_db_dir), to keep rules there).
    pub fn reason_db_dir(&self) -> Option<PathBuf> {
        self.reason_home.dir.clone().or_else(|| self.home.clone())
    }

    /// Keep this database's reasoning rules in `<dir>/reason_rule_files/`
    /// (a server sets it to the directory it serves the database from, which
    /// a database built in memory and saved there does not know as its
    /// [`home`](Self::home)). Rules held in memory until now are not moved.
    pub fn set_reason_db_dir(&mut self, dir: impl Into<PathBuf>) {
        self.reason_home.dir = Some(dir.into());
    }

    /// Run `f` with this database and its rule store.
    fn with_rule_store<R>(
        &mut self,
        f: impl FnOnce(&mut Database, &mut crate::reason::store::RuleStore<'_>) -> R,
    ) -> R {
        use crate::reason::store::RuleStore;
        match self.reason_db_dir() {
            Some(dir) => f(self, &mut RuleStore::dir(dir)),
            None => {
                let mut mem = std::mem::take(&mut self.reason_home.mem);
                let out = f(self, &mut RuleStore::Memory(&mut mem));
                self.reason_home.mem = mem;
                out
            }
        }
    }

    /// Add an upstream reasoning rule (`reasonManage` type 1) from its JSON
    /// object (`rulename`, `type`, `logic`, `conditions`, `return`, …; see
    /// [`crate::reason::rules`]). It is validated — so that it can never
    /// inject SPARQL — and stored with status `新建`.
    pub fn reason_add(
        &mut self,
        ruleinfo: &crate::json::Json,
    ) -> std::result::Result<crate::reason::manage::Added, crate::reason::ReasonError> {
        let now = crate::sysdb::clock::now().datetime();
        self.with_rule_store(|_, store| crate::reason::manage::add(store, ruleinfo, &now))
    }

    /// Every reasoning rule's JSON, by name (type 2).
    pub fn reason_list(
        &mut self,
    ) -> std::result::Result<Vec<crate::json::Json>, crate::reason::ReasonError> {
        self.with_rule_store(|_, store| crate::reason::manage::list(store))
    }

    /// Compile a rule to its SPARQL statements (type 3; status `已编译`).
    pub fn reason_compile(
        &mut self,
        name: &str,
    ) -> std::result::Result<crate::reason::CompiledRule, crate::reason::ReasonError> {
        self.with_rule_store(|_, store| crate::reason::manage::compile(store, name))
    }

    /// Execute a compiled rule (type 4; status `已执行`): its `INSERT … WHERE`
    /// runs through the normal update path, and the triples it infers are
    /// recorded so that [`reason_disable`](Self::reason_disable) retracts
    /// exactly them. Refused inside a transaction.
    pub fn reason_execute(
        &mut self,
        name: &str,
    ) -> std::result::Result<crate::reason::manage::Executed, crate::reason::ReasonError> {
        self.with_rule_store(|db, store| crate::reason::manage::execute(db, store, name, None))
    }

    /// Disable a compiled rule (type 5; status `已失效`), retracting the
    /// triples it inferred that no other rule also inferred. Refused inside
    /// a transaction.
    pub fn reason_disable(
        &mut self,
        name: &str,
    ) -> std::result::Result<crate::reason::manage::Disabled, crate::reason::ReasonError> {
        self.with_rule_store(|db, store| crate::reason::manage::disable(db, store, name, None))
    }

    /// A rule's JSON (type 6).
    pub fn reason_show(
        &mut self,
        name: &str,
    ) -> std::result::Result<crate::json::Json, crate::reason::ReasonError> {
        self.with_rule_store(|_, store| crate::reason::manage::show(store, name))
    }

    /// Delete a rule that is not executed (type 7); returns upstream's
    /// message.
    pub fn reason_delete(
        &mut self,
        name: &str,
    ) -> std::result::Result<String, crate::reason::ReasonError> {
        self.with_rule_store(|_, store| crate::reason::manage::delete(store, name))
    }

    /// Count a compiled rule's matches (type 8; `effectNum`, status
    /// `已校验` unless executed).
    pub fn reason_check(
        &mut self,
        name: &str,
    ) -> std::result::Result<crate::reason::manage::Checked, crate::reason::ReasonError> {
        self.with_rule_store(|db, store| {
            crate::reason::manage::check(store, name, |sparql| db.query(sparql))
        })
    }

    /// Whether the default graph holds `t` (no interning).
    pub(crate) fn contains_triple(&self, t: &Triple) -> bool {
        let (Some(sub), Some(pred), Some(obj)) = (
            self.dict.entity_id(&t.subject.dict_key()),
            self.dict.predicate_id(&t.predicate.dict_key()),
            self.dict.term_id(&t.object),
        ) else {
            return false;
        };
        self.store.exists(sub, pred, obj)
    }

    /// Run a rule's compiled `INSERT … WHERE` (one operation, default graph)
    /// through the update path, reporting which triples it derived, which
    /// of them existed before, and which it added. An error before anything
    /// was applied (parse, WHERE evaluation, cancellation) is `Err`; the
    /// update's own outcome is the report's `result`.
    pub(crate) fn reason_infer(
        &mut self,
        sparql: &str,
        cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<crate::reason::manage::InferReport> {
        use crate::reason::manage::{ledger_line, InferReport};
        let Query::Update(ops) = sparql::parse(sparql)? else {
            return Err(GStoreError::Query(
                "a reasoning rule must compile to an update".into(),
            ));
        };
        let [UpdateOp::Modify {
            with: None,
            delete,
            insert,
            using,
            using_named,
            pattern,
        }] = ops.as_slice()
        else {
            return Err(GStoreError::Query(
                "a reasoning rule must compile to one INSERT … WHERE".into(),
            ));
        };
        let (_, ins) = self.modify_changes(
            delete,
            insert,
            pattern,
            None,
            using,
            using_named,
            cancel.as_ref(),
        )?;
        let mut seen = HashSet::new();
        let mut derived: Vec<(String, bool, Triple)> = Vec::new();
        for (g, t) in ins {
            if g.is_some() {
                continue;
            }
            let line = ledger_line(&t);
            if seen.insert(line.clone()) {
                let existed = self.contains_triple(&t);
                derived.push((line, existed, t));
            }
        }
        let result = match self.run_update(sparql, ops, cancel) {
            Ok(QueryResult::Update { changed }) => Ok(changed),
            Ok(_) => Ok(0),
            Err(e) => Err(e.to_string()),
        };
        let inserted = derived
            .iter()
            .filter(|(_, existed, t)| !existed && self.contains_triple(t))
            .map(|(line, _, _)| line.clone())
            .collect();
        Ok(InferReport {
            derived: derived.into_iter().map(|(l, e, _)| (l, e)).collect(),
            inserted,
            result,
        })
    }

    /// Retract the triples of ledger `lines` (see
    /// [`crate::reason::manage`]) as one `DELETE DATA` through the update
    /// path. Returns how many were removed. The opt-in statement log
    /// (`update.log`) records that `DELETE DATA` text; a blank node in it is
    /// written as its stored label, which SPARQL does not accept in `DELETE
    /// DATA`, so replaying such a record fails (the triple-level redo log is
    /// unaffected).
    pub(crate) fn reason_retract(
        &mut self,
        lines: &[String],
        cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<usize> {
        let mut ground = Vec::with_capacity(lines.len());
        let mut text = String::from("DELETE DATA {");
        for line in lines {
            let Some(t) = crate::reason::manage::parse_ledger_line(line) else {
                continue;
            };
            text.push_str(&format!(" {} {} {} .", t.subject, t.predicate, t.object));
            ground.push(GroundTriple {
                subject: t.subject,
                predicate: t.predicate,
                object: t.object,
                graph: None,
            });
        }
        text.push_str(" }");
        if ground.is_empty() {
            return Ok(0);
        }
        match self.run_update(&text, vec![UpdateOp::DeleteData(ground)], cancel)? {
            QueryResult::Update { changed } => Ok(changed),
            _ => Ok(0),
        }
    }

    /// Run SPARQL UPDATE `sparql` (a rule's compiled statement) through the
    /// update path; the number of triples changed.
    pub(crate) fn reason_update(
        &mut self,
        sparql: &str,
        cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<usize> {
        let Query::Update(ops) = sparql::parse(sparql)? else {
            return Err(GStoreError::Query("not an update".into()));
        };
        match self.run_update(sparql, ops, cancel)? {
            QueryResult::Update { changed } => Ok(changed),
            _ => Ok(0),
        }
    }

    /// Carry this database's reasoning rules into backup directory `dst`
    /// (upstream's backup copies the whole database directory).
    fn backup_reason_rules(&self, dst: &Path) -> Result<()> {
        match self.reason_db_dir() {
            Some(src) => crate::reason::store::copy_rule_dir(&src, dst),
            None => crate::reason::store::write_memory_rules(&self.reason_home.mem, dst),
        }
    }

    // ---- query cache / evaluation options ---------------------------------

    /// Set the read-query cache's memory budget in (approximate) bytes,
    /// clearing it; 0 disables caching. The default is
    /// [`DEFAULT_QUERY_CACHE_BYTES`] (64 MiB). A single result larger than
    /// an eighth of the budget is never cached; least-recently-used results
    /// are evicted to make room. Only deterministic read queries are cached
    /// (none calling `NOW()`, `RAND()`, `UUID()`, `STRUUID()`, `BNODE()`, a
    /// user function, or using `SERVICE`), and every data change empties the
    /// cache. Upstream gStore hard-codes its budget (100 MB); the servers set
    /// this from `conf.ini`'s `[query] query_cache_size` (bytes, see
    /// [`QuerySettings`]).
    pub fn set_query_cache_size(&mut self, bytes: usize) {
        *self.query_cache.get_mut() = QueryCache::new(bytes);
    }

    /// The read-query cache's budget in bytes (see
    /// [`set_query_cache_size`](Self::set_query_cache_size)).
    pub fn query_cache_size(&self) -> usize {
        self.query_cache.borrow().budget()
    }

    /// Number of results currently in the read-query cache.
    pub fn query_cache_len(&self) -> usize {
        self.query_cache.borrow().len()
    }

    /// Approximate bytes the read-query cache currently holds.
    pub fn query_cache_bytes(&self) -> usize {
        self.query_cache.borrow().bytes()
    }

    /// Choose how `/` divides two integers in this database's queries and
    /// updates: [`IntegerDivision::Decimal`] (the default, SPARQL 1.1:
    /// `7/2` = `3.5`) or [`IntegerDivision::Truncate`] (upstream gStore
    /// compatibility: `7/2` = `3`, and `AVG` over integers truncates).
    /// Clears the query cache. The servers set this from `conf.ini`'s
    /// `[query] integer_division` (`decimal` | `truncate`, see
    /// [`IntegerDivision::parse`] and [`QuerySettings`]).
    pub fn set_integer_division(&mut self, mode: IntegerDivision) {
        if self.integer_division != mode {
            self.integer_division = mode;
            self.query_cache.get_mut().clear();
        }
    }

    /// The current integer-division mode (see
    /// [`set_integer_division`](Self::set_integer_division)).
    pub fn integer_division(&self) -> IntegerDivision {
        self.integer_division
    }

    /// Choose which `LIMIT` / `ORDER BY` / `ASK` / `EXISTS` optimizations
    /// this database's queries may use: early termination, the bounded-heap
    /// Top-K and DP-B (see [`TopKOptions`]; all on by default). Every one
    /// returns exactly the rows the full evaluation returns, so this only
    /// changes how much work a query does — it exists for testing and
    /// benchmarking ([`TopKOptions::NONE`] evaluates every query in full).
    /// Clears the query cache, so the next query really runs with the new
    /// options. The servers set this from `conf.ini`'s `[query]` keys
    /// `early_termination`, `topk_bounded_heap` and `topk_dpb` (see
    /// [`QuerySettings`]).
    pub fn set_topk_options(&mut self, options: TopKOptions) {
        if self.topk != options {
            self.topk = options;
            self.query_cache.get_mut().clear();
        }
    }

    /// The current Top-K / early-termination options (see
    /// [`set_topk_options`](Self::set_topk_options)).
    pub fn topk_options(&self) -> TopKOptions {
        self.topk
    }

    /// Bound the JSON one call of a gStore advanced graph function
    /// (`shortestPaths(…)`, `kHopEnumerate(…)`, `PR(…)`, …) may produce in
    /// this database's queries: a call that would exceed it fails with a
    /// query error instead of building an unbounded result (upstream has no
    /// bound). The default is
    /// [`paths::MAX_OUTPUT_BYTES`](crate::analytics::gfun::paths::MAX_OUTPUT_BYTES)
    /// (256 MiB); the servers apply `conf.ini`'s `gfun_max_output_bytes`
    /// (64 MiB by default, see [`crate::config`]). Clears the query cache.
    pub fn set_gfun_max_output_bytes(&mut self, bytes: usize) {
        if self.gfun_max_output_bytes != bytes {
            self.gfun_max_output_bytes = bytes;
            self.query_cache.get_mut().clear();
        }
    }

    /// The graph-function output bound (see
    /// [`set_gfun_max_output_bytes`](Self::set_gfun_max_output_bytes)).
    pub fn gfun_max_output_bytes(&self) -> usize {
        self.gfun_max_output_bytes
    }

    /// Let this database's queries call `caller`'s PFN functions
    /// (`PFN.name({…})`, see [`crate::pfn`]); `None` (the default) makes a PFN
    /// call a query error. For an embedded database this is the one caller
    /// every query runs as; a server leaves it unset and passes the
    /// authenticated user with each query ([`query_as`](Self::query_as)).
    /// [`ConcurrentDb`](crate::ConcurrentDb) snapshots carry it. The caller's
    /// limits are replaced by this database's
    /// ([`set_pfn_limits`](Self::set_pfn_limits)) when a query runs.
    pub fn set_pfn_caller(&mut self, caller: Option<crate::pfn::PfnCaller>) {
        self.pfn = caller;
    }

    /// The installed PFN caller (see [`set_pfn_caller`](Self::set_pfn_caller)),
    /// with this database's PFN limits.
    pub fn pfn_caller(&self) -> Option<crate::pfn::PfnCaller> {
        self.pfn.clone().map(|c| c.with_limits(self.pfn_limits))
    }

    /// The sandbox limits PFN calls of this database's queries run under
    /// (instruction budget, memory; `conf.ini`'s `[pfn]` section). PFN results
    /// are never cached, so no cache needs clearing.
    pub fn set_pfn_limits(&mut self, limits: crate::pfn::PfnLimits) {
        self.pfn_limits = limits;
    }

    /// The PFN limits (see [`set_pfn_limits`](Self::set_pfn_limits)).
    pub fn pfn_limits(&self) -> crate::pfn::PfnLimits {
        self.pfn_limits
    }

    /// Apply a server's query-evaluation settings ([`QuerySettings`], from
    /// `conf.ini`'s `[query]` and `[pfn]` sections): integer-division mode,
    /// query-cache budget, Top-K / early-termination options, graph-function
    /// output bound and PFN limits. Each setter clears the query cache only
    /// when its value changes.
    pub fn apply_query_settings(&mut self, settings: &QuerySettings) {
        self.set_integer_division(settings.integer_division);
        if self.query_cache_size() != settings.query_cache_bytes {
            self.set_query_cache_size(settings.query_cache_bytes);
        }
        self.set_topk_options(settings.topk);
        self.set_gfun_max_output_bytes(settings.gfun_max_output_bytes);
        self.set_pfn_limits(settings.pfn);
    }

    /// This database's current query-evaluation settings (see
    /// [`apply_query_settings`](Self::apply_query_settings)).
    pub fn query_settings(&self) -> QuerySettings {
        QuerySettings {
            integer_division: self.integer_division,
            query_cache_bytes: self.query_cache_size(),
            topk: self.topk,
            gfun_max_output_bytes: self.gfun_max_output_bytes,
            pfn: self.pfn_limits,
        }
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
            let id = db.encode_triple(&t)?;
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
                ids.push(db.encode_triple(&t)?);
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

/// Parse `key <u64>` out of a simple `MAGIC 1\n<key> <value>\n…` marker text,
/// returning the value for `key`. Used by the backup-marker readers below.
fn marker_field(text: &str, magic: &str, key: &str) -> Result<u64> {
    let mut lines = text.lines();
    let header = lines.next().unwrap_or_default();
    if header.split_whitespace().next() != Some(magic) {
        return Err(GStoreError::Database(format!(
            "not a {magic} marker (bad header {header:?})"
        )));
    }
    for line in lines {
        let mut it = line.split_whitespace();
        if it.next() == Some(key) {
            return it
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| GStoreError::Database(format!("marker field '{key}' is not a u64")));
        }
    }
    Err(GStoreError::Database(format!(
        "marker field '{key}' not found"
    )))
}

/// Write a full-backup checkpoint marker: `GSTORE-BACKUP 1\noffset <N>\n`.
fn write_backup_offset(path: &Path, offset: u64) -> Result<()> {
    fsutil::atomic_write(
        path,
        format!("GSTORE-BACKUP 1\noffset {offset}\n").as_bytes(),
    )
}

/// Read a full-backup checkpoint marker's `offset`.
fn read_backup_offset(path: &Path) -> Result<u64> {
    let text = fs::read_to_string(path)?;
    marker_field(&text, "GSTORE-BACKUP", "offset")
}

/// Write an incremental-backup marker:
/// `GSTORE-INCREMENTAL 1\nbase_offset <B>\nend_offset <E>\n`.
fn write_incremental_marker(path: &Path, base: u64, end: u64) -> Result<()> {
    fsutil::atomic_write(
        path,
        format!("GSTORE-INCREMENTAL 1\nbase_offset {base}\nend_offset {end}\n").as_bytes(),
    )
}

/// Read an incremental-backup marker's `(base_offset, end_offset)`.
fn read_incremental_marker(path: &Path) -> Result<(u64, u64)> {
    let text = fs::read_to_string(path)?;
    let base = marker_field(&text, "GSTORE-INCREMENTAL", "base_offset")?;
    let end = marker_field(&text, "GSTORE-INCREMENTAL", "end_offset")?;
    Ok((base, end))
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

    /// An UPDATE's WHERE clause runs under the request's cancel flag, like a
    /// read: a graph function in a sub-SELECT there (which may run for as
    /// long as its `maxIter` says) is stopped by it, and a cancelled update
    /// applies nothing — whichever dataset (`WITH`, `USING`, default) the
    /// WHERE clause matches.
    #[test]
    fn updates_honour_the_cancel_flag() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let mut db = Database::build_from_str("t", "").unwrap();
        db.query(
            "INSERT DATA { <http://ex/a> <http://ex/p> <http://ex/b> . \
             <http://ex/b> <http://ex/p> <http://ex/c> . <http://ex/c> <http://ex/p> <http://ex/a> . \
             GRAPH <http://ex/g> { <http://ex/a> <http://ex/p> <http://ex/b> } }",
        )
        .unwrap();
        let before = db.mutation_count();
        let sub = |iters: &str| {
            format!(
                "{{ SELECT (PR(true, {{}}, 3, alpha=0.85, maxIter={iters}, tol=-1) AS ?r) WHERE {{}} }}"
            )
        };
        let forms = |iters: &str| {
            let s = sub(iters);
            [
                format!("INSERT {{ <http://ex/x> <http://ex/pr> ?r }} WHERE {{ {s} }}"),
                format!("WITH <http://ex/g> INSERT {{ <http://ex/x> <http://ex/pr> ?r }} WHERE {{ {s} }}"),
                format!(
                    "INSERT {{ <http://ex/x> <http://ex/pr> ?r }} USING <http://ex/g> WHERE {{ {s} }}"
                ),
                format!("DELETE {{ ?s ?p ?o }} WHERE {{ ?s ?p ?o {s} }}"),
            ]
        };
        // Flag already up: refused before anything is applied.
        for u in forms("5") {
            let r = db.query_cancellable(&u, Arc::new(AtomicBool::new(true)));
            assert!(matches!(r, Err(GStoreError::Cancelled)), "{u}: {r:?}");
            assert_eq!(db.mutation_count(), before, "{u}");
        }
        // Raised while a never-converging PageRank runs in the WHERE clause.
        let u = &forms("2000000000")[0];
        let flag = Arc::new(AtomicBool::new(false));
        let raise = Arc::clone(&flag);
        let stopper = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            raise.store(true, Ordering::Relaxed);
        });
        let started = std::time::Instant::now();
        let r = db.query_cancellable(u, flag);
        stopper.join().unwrap();
        assert!(matches!(r, Err(GStoreError::Cancelled)), "{r:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(30));
        assert_eq!(db.mutation_count(), before);
        // Without a cancel the same statements run.
        let r = db.query(&forms("5")[0]).unwrap();
        assert!(matches!(r, QueryResult::Update { changed: 1 }), "{r:?}");
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

    // ---- full + incremental backup / restore -----------------------------

    /// Every `(s, p, o)` triple in the default graph, sorted — used to compare
    /// the exact triple set of two databases.
    fn sorted_triples(db: &mut Database) -> Vec<Vec<Option<String>>> {
        let mut rows = db
            .select("SELECT ?s ?p ?o WHERE { ?s ?p ?o }")
            .unwrap()
            .rows;
        rows.sort();
        rows
    }

    #[test]
    fn full_then_incremental_backup_restore_roundtrip() {
        let base_dir = temp_dir("bk_base");
        let incr1_dir = temp_dir("bk_incr1");
        let incr2_dir = temp_dir("bk_incr2");
        let db_dir = temp_dir("bk_db");

        // Live database with redo logging enabled inside its own directory.
        let mut db = Database::build_from_str("bk", SMALL).unwrap();
        db.enable_redo_log_in(&db_dir).unwrap();

        // An update BEFORE the full backup: captured by the base snapshot itself.
        db.query("INSERT DATA { <root> <contain> <n_pre> }").unwrap();

        // Full backup: base snapshot + checkpoint at the current redo-log length.
        db.backup(&base_dir).unwrap();
        let base_ck = db.backup_checkpoint();
        assert!(base_ck > 0, "checkpoint records the pre-backup update");

        // Updates AFTER the full backup ⇒ first increment (insert + delete).
        db.query("INSERT DATA { <root> <contain> <n1> . <n1> <own> <p1> }")
            .unwrap();
        db.query("DELETE DATA { <root> <contain> <node0> }").unwrap();
        db.backup_incremental(&incr1_dir).unwrap();
        let ck1 = db.backup_checkpoint();
        assert!(ck1 > base_ck, "checkpoint advanced past the base");

        // More updates ⇒ second increment, contiguous with the first.
        db.query("INSERT DATA { <n1> <own> <p2> }").unwrap();
        db.backup_incremental(&incr2_dir).unwrap();
        assert!(db.backup_checkpoint() > ck1);

        // The live database's exact triple set.
        let live = sorted_triples(&mut db);
        let live_count = db.triple_num();

        // Restore base + both increments in order ⇒ exact reproduction.
        let mut restored =
            Database::restore_incremental(&base_dir, &[&incr1_dir, &incr2_dir]).unwrap();
        assert_eq!(
            sorted_triples(&mut restored),
            live,
            "base + increments reproduce the exact triple set"
        );
        assert_eq!(restored.triple_num(), live_count);
        // The post-backup insert and delete are both reflected.
        assert_eq!(
            restored
                .select("SELECT ?o WHERE { <n1> <own> ?o }")
                .unwrap()
                .row_count(),
            2,
            "both p1 and p2 restored"
        );
        assert_eq!(
            restored
                .select("SELECT ?o WHERE { <root> <contain> <node0> }")
                .unwrap()
                .row_count(),
            0,
            "the incremental delete is applied"
        );

        // Restoring ONLY the base reproduces the pre-increment state.
        let mut base_only = Database::restore(&base_dir).unwrap();
        assert!(base_only.triple_num() < live_count, "base predates the increments");
        assert_eq!(
            base_only
                .select("SELECT ?o WHERE { <n1> <own> ?o }")
                .unwrap()
                .row_count(),
            0,
            "the base does not contain post-backup inserts"
        );

        // An out-of-order increment chain is rejected (gap/contiguity check).
        let err = Database::restore_incremental(&base_dir, &[&incr2_dir, &incr1_dir]).unwrap_err();
        assert!(
            matches!(err, GStoreError::Database(_)),
            "out-of-order increments must be rejected, got {err:?}"
        );

        for d in [&base_dir, &incr1_dir, &incr2_dir, &db_dir] {
            let _ = fs::remove_dir_all(d);
        }
    }

    #[test]
    fn backup_incremental_requires_redo_log() {
        let db = Database::build_from_str("bk", SMALL).unwrap();
        let out = temp_dir("bk_no_log");
        let err = db.backup_incremental(&out).unwrap_err();
        assert!(
            matches!(err, GStoreError::Database(_)),
            "incremental backup without a redo log must error, got {err:?}"
        );
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn restore_incremental_rejects_corrupt_base_marker() {
        // #7 (LOW): a base whose BACKUP_OFFSET marker exists but cannot be parsed
        // must fail the restore rather than silently defaulting the checkpoint to 0
        // (which would accept a mis-aligned increment chain).
        let base_dir = temp_dir("bk_corrupt_base");
        let incr_dir = temp_dir("bk_corrupt_incr");
        let db_dir = temp_dir("bk_corrupt_db");

        let mut db = Database::build_from_str("bk", SMALL).unwrap();
        db.enable_redo_log_in(&db_dir).unwrap();
        db.query("INSERT DATA { <root> <contain> <n_pre> }").unwrap();
        db.backup(&base_dir).unwrap();
        db.query("INSERT DATA { <root> <contain> <n1> }").unwrap();
        db.backup_incremental(&incr_dir).unwrap();

        // Present-but-unparseable marker (not an absent one, which is a legit 0).
        fs::write(base_dir.join(BACKUP_OFFSET_FILE), b"garbage not a marker\n").unwrap();
        let err = Database::restore_incremental(&base_dir, &[&incr_dir]).unwrap_err();
        assert!(
            matches!(err, GStoreError::Database(_)),
            "a corrupt base checkpoint marker must error, got {err:?}"
        );

        for d in [&base_dir, &incr_dir, &db_dir] {
            let _ = fs::remove_dir_all(d);
        }
    }

    #[test]
    fn incremental_backup_excludes_rolled_back_and_captures_direct_api() {
        // Regression for the redo-log re-basing (findings #8/#12): a rolled-back
        // transaction must NOT appear in the increment, and a direct-API mutation
        // (no SPARQL statement) MUST appear.
        let base_dir = temp_dir("bk_rb_base");
        let incr_dir = temp_dir("bk_rb_incr");
        let db_dir = temp_dir("bk_rb_db");

        let mut db = Database::build_from_str("bk_rb", SMALL).unwrap();
        db.enable_redo_log_in(&db_dir).unwrap();
        db.backup(&base_dir).unwrap();

        // (a) direct-API insert (bypasses the SPARQL update path entirely)
        db.insert_triple(&Triple::new(Term::iri("root"), Term::iri("contain"), Term::iri("api_node")));
        // (b) a transaction that is rolled back — must leave no trace
        db.begin().unwrap();
        db.query("INSERT DATA { <root> <contain> <ghost> }").unwrap();
        db.rollback().unwrap();

        db.backup_incremental(&incr_dir).unwrap();
        let mut restored = Database::restore_incremental(&base_dir, &[&incr_dir]).unwrap();

        assert_eq!(
            restored.select("SELECT ?s WHERE { ?s <contain> <api_node> }").unwrap().row_count(),
            1,
            "direct-API insert must be captured by the redo-based increment"
        );
        assert_eq!(
            restored.select("SELECT ?s WHERE { ?s <contain> <ghost> }").unwrap().row_count(),
            0,
            "a rolled-back transaction must NOT be resurrected by restore"
        );
        for d in [&base_dir, &incr_dir, &db_dir] {
            let _ = fs::remove_dir_all(d);
        }
    }

    #[test]
    fn successive_incrementals_are_contiguous() {
        // Three increments back to back with no intervening full backup each pick
        // up exactly where the previous ended; restore replays the whole chain.
        let base_dir = temp_dir("bk2_base");
        let db_dir = temp_dir("bk2_db");
        let incr: Vec<_> = (0..3).map(|i| temp_dir(&format!("bk2_incr{i}"))).collect();

        let mut db = Database::build_from_str("bk2", SMALL).unwrap();
        db.enable_redo_log_in(&db_dir).unwrap();
        db.backup(&base_dir).unwrap();

        let mut prev_end = db.backup_checkpoint();
        for (i, dir) in incr.iter().enumerate() {
            db.query(&format!("INSERT DATA {{ <root> <seq> <s{i}> }}"))
                .unwrap();
            db.backup_incremental(dir).unwrap();
            let (base, end) = read_incremental_marker(&dir.join(INCREMENTAL_MARKER_FILE)).unwrap();
            assert_eq!(base, prev_end, "increment {i} starts where the last ended");
            assert!(end > base, "increment {i} covers new records");
            prev_end = end;
        }

        let refs: Vec<&std::path::PathBuf> = incr.iter().collect();
        let mut restored = Database::restore_incremental(&base_dir, &refs).unwrap();
        assert_eq!(
            restored
                .select("SELECT ?o WHERE { <root> <seq> ?o }")
                .unwrap()
                .row_count(),
            3,
            "all three sequential increments were replayed"
        );

        let _ = fs::remove_dir_all(&base_dir);
        let _ = fs::remove_dir_all(&db_dir);
        for d in &incr {
            let _ = fs::remove_dir_all(d);
        }
    }

    // ---- query-cache LRU -------------------------------------------------

    #[test]
    fn query_cache_lru_evicts_least_recently_used() {
        let mut db = Database::build_from_str("lru", SMALL).unwrap();
        // Queries q(10)..q(19) have results of exactly the same size.
        let q = |i: usize| format!("SELECT ?o{i} WHERE {{ <root> <contain> ?o{i} }}");
        db.query(&q(10)).unwrap();
        let one = db.query_cache_bytes();
        // The cache is bounded by memory: room for exactly eight results.
        db.set_query_cache_size(8 * one);
        assert_eq!(db.query_cache_len(), 0, "resizing clears the cache");
        for i in 10..18 {
            db.query(&q(i)).unwrap();
        }
        assert_eq!(db.query_cache_len(), 8);
        // Touch q(10) so q(11) is now the least-recently used.
        db.query(&q(10)).unwrap();
        // Inserting q(18) evicts q(11).
        db.query(&q(18)).unwrap();
        assert_eq!(db.query_cache_len(), 8, "cache stays within its budget");
        assert!(db.query_cache_bytes() <= db.query_cache_size());
        assert!(db.query_cache_contains(&q(10)));
        assert!(db.query_cache_contains(&q(18)));
        assert!(
            !db.query_cache_contains(&q(11)),
            "q(11) was least-recently used"
        );
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

    /// An **in-memory** database can opt into an out-of-core (streaming) VS-tree
    /// and answer queries identically to the default in-memory tree, while the
    /// filter streams nodes from disk (proving the whole tree need not be
    /// resident). This is the in-memory query path using the out-of-core seam.
    #[test]
    fn streaming_vstree_matches_in_memory_path() {
        let mut content = String::new();
        for i in 0..400u32 {
            content.push_str(&format!("<http://ex/s{i}> <http://ex/p> <http://ex/o{i}> .\n"));
        }
        content.push_str("<http://ex/special> <http://ex/marker> <http://ex/target> .\n");

        // Baseline: default in-memory VS-tree.
        let mut base = Database::build_from_str("mem", &content).unwrap();
        assert!(!base.vstree.is_disk_backed());

        // Same data, then opt into the streaming (out-of-core) VS-tree.
        let mut streamed = Database::build_from_str("stream", &content).unwrap();
        let dir = temp_dir("stream_vstree");
        let path = dir.join(VSTREE_KV_FILE);
        streamed.use_streaming_vstree(&path).unwrap();
        assert!(streamed.vstree.is_disk_backed(), "opted into out-of-core VS-tree");
        assert!(streamed.stats().index_valid, "streaming index is valid for filtering");
        // The store stayed in memory; only the filter went out-of-core.
        assert!(matches!(streamed.store, Backend::Memory(_)));

        for q in [
            "SELECT ?s WHERE { ?s <http://ex/marker> <http://ex/target> }",
            "SELECT ?o WHERE { <http://ex/s5> <http://ex/p> ?o }",
            "SELECT ?s ?o WHERE { ?s <http://ex/p> ?o }",
            "SELECT ?s WHERE { ?s <http://ex/p> <http://ex/o42> }",
        ] {
            let mut a = streamed.select(q).unwrap().rows;
            let mut b = base.select(q).unwrap().rows;
            a.sort();
            b.sort();
            assert_eq!(a, b, "streaming-vstree query {q:?} diverged from in-memory");
        }
        // The candidate filter actually paged VS-tree nodes off disk.
        assert!(
            streamed.vstree.disk_pages_read() > 0,
            "streaming VS-tree was traversed on disk"
        );
        let _ = fs::remove_dir_all(&dir);
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
