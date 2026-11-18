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

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::analytics::csr::{CsrCache, CsrGraph};
use crate::dict::{Dictionary, TermStr};
use crate::error::{GStoreError, Result};
use crate::kvstore::StringIndex;
use crate::model::id::{is_entity_id, EntityLiteralId};
use crate::model::term::record_key;
use crate::model::{IdTriple, Term, Triple};
use crate::parser::import;
use crate::parser::nquads::Quad;
use crate::parser::sparql;
use crate::parser::sparql::ast::{
    GraphPattern, GraphTarget, GroundTriple, QuadPattern, Query, UpdateOp, RDF_TYPE,
};
use crate::query::engine::{dictionary_string_index_cancellable, EvalProviders};
use crate::query::{Evaluator, FunctionRegistry, IntegerDivision, QueryResult, TopKOptions};
use crate::signature::{EdgeDir, Signature, VsTree};
use crate::store::read_scope::ReadScope;
use crate::store::{Backend, MutableStore, TripleSource, TripleStore};

mod admin;
pub mod dirlock;
pub(crate) mod fsutil;
mod logs;
use logs::LogKind;
mod open_options;
pub(crate) mod persist;
mod statement;
pub use open_options::{
    default_open_options, set_default_open_options, DiskQueryMode, OpenOptions,
    DEFAULT_DICT_CACHE_BYTES, DEFAULT_DISK_CACHE_PAGES, DEFAULT_DISK_QUERY_MODE, ENV_DISK_CACHE_MB,
    ENV_DISK_QUERY_MODE, ENV_VSTREE_ON_OPEN,
};
pub use statement::Statement;

#[cfg(all(test, feature = "rocksdb"))]
mod commit_fate_tests;
#[cfg(test)]
mod concurrency_tests;

pub(crate) use admin::{class_stats_of, ClassStats};
pub use admin::{SchemaEdge, SchemaGraph, SchemaNode};
pub use persist::StorageKind;

#[cfg(test)]
mod hardening_tests;
#[cfg(test)]
mod layout_matrix_tests;
mod load_policy;
pub mod named;
#[cfg(test)]
mod r4_review_tests;
pub mod reclaim;
#[cfg(test)]
mod redo_backup_tests;
#[cfg(test)]
mod review_tests;
#[cfg(all(test, feature = "rocksdb"))]
mod rocks_recovery_tests;
#[cfg(test)]
mod stream_review_tests;
pub use load_policy::{LoadPolicy, LoadSource, ServicePolicy};
mod query_cache;
use named::NamedGraphs;
use query_cache::QueryCache;
pub use query_cache::DEFAULT_QUERY_CACHE_BYTES;
pub use reclaim::{
    default_id_reclaim, set_default_id_reclaim, IdReclaim, IdSpaceStats, ENV_ID_RECLAIM,
};
use reclaim::{Candidates, Deferred};

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
/// Marker file inside a *full* backup directory recording the redo-log
/// checkpoint (byte length) captured at backup time. Lets a later incremental
/// backup know where its delta begins; ignored by [`Database::load`].
const BACKUP_OFFSET_FILE: &str = "BACKUP_OFFSET";
/// Marker file inside an *incremental* backup directory describing its delta
/// (the base/end redo-log byte offsets it covers).
const INCREMENTAL_MARKER_FILE: &str = "INCREMENTAL";
/// The copied slice of the redo log inside an incremental backup directory —
/// its whole records for `[base_offset, end_offset)`.
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

/// [`vstree_entries`] for the in-memory store, from one merged walk of its
/// subject and object maps (no per-entity lookup or copy). Equal to the
/// generic version.
fn memory_vstree_entries(store: &TripleStore) -> Vec<(EntityLiteralId, Signature)> {
    let mut out = Vec::new();
    store.for_each_entity_adjacency(|e, outgoing, incoming| {
        let mut sig = Signature::new();
        for &(p, o) in outgoing {
            sig.encode_edge(p, o, EdgeDir::Out);
        }
        for &(p, s) in incoming {
            sig.encode_edge(p, s, EdgeDir::In);
        }
        out.push((e, sig));
    });
    out
}

/// [`vstree_entries`] for the on-disk store, from one streaming pass over its
/// SPO and OSP trees ([`DiskStore::for_each_entity_adjacency`]) instead of
/// two range reads and copies per entity. Equal to the generic version; a
/// read error fails the build instead of signing an entity with missing edges.
///
/// [`DiskStore::for_each_entity_adjacency`]: crate::kvstore::DiskStore::for_each_entity_adjacency
fn disk_vstree_entries(
    store: &crate::kvstore::DiskStore,
) -> Result<Vec<(EntityLiteralId, Signature)>> {
    let mut out = Vec::new();
    store.for_each_entity_adjacency(|e, outgoing, incoming| {
        let mut sig = Signature::new();
        for &(p, o) in outgoing {
            sig.encode_edge(p, o, EdgeDir::Out);
        }
        for &(p, s) in incoming {
            sig.encode_edge(p, s, EdgeDir::In);
        }
        out.push((e, sig));
        std::ops::ControlFlow::Continue(())
    })?;
    Ok(out)
}

/// [`vstree_entries`] of a [`Backend`], through the in-memory fast path when
/// it holds a [`TripleStore`], and the streaming pass over the committed
/// on-disk store when it holds a [`DiskTriples`](crate::kvstore::DiskTriples)
/// without an open transaction (a read error there is reported to the
/// thread's read scope and the generic signing runs instead).
fn backend_vstree_entries(store: &Backend) -> Vec<(EntityLiteralId, Signature)> {
    if let Some(mem) = store.as_memory() {
        return memory_vstree_entries(mem);
    }
    if let Some(dt) = store.as_disk().filter(|dt| !dt.in_transaction()) {
        match dt.disk_store().and_then(disk_vstree_entries) {
            Ok(entries) => return entries,
            Err(e) => crate::store::read_scope::note_read_error(format_args!(
                "signing the on-disk store's entities: {e}"
            )),
        }
    }
    vstree_entries(store)
}

#[cfg(test)]
thread_local! {
    /// Edges read to re-sign entities from the store on this thread (tests
    /// bound the incremental maintenance's work with it).
    static RESIGNED_EDGES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Build an in-memory VS-tree over every entity (see [`vstree_entries`]).
fn build_vstree(store: &Backend) -> VsTree {
    VsTree::build(backend_vstree_entries(store))
}

/// [`backend_vstree_entries`] in a read scope (C2): a read error fails it
/// instead of signing an entity with missing edges — a tree built from such
/// entries is not a superset, and queries filtering through it would
/// silently lose rows.
fn checked_vstree_entries(store: &Backend) -> Result<Vec<(EntityLiteralId, Signature)>> {
    let scope = ReadScope::enter();
    let entries = backend_vstree_entries(store);
    match scope.take_error() {
        Some(e) => Err(read_failure("signing the entities for the VS-tree", e)),
        None => Ok(entries),
    }
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

/// One pending redo-log record: an applied mutation in surface form (framed
/// by [`logs::frame_redo`]). `op` is `b'I'` (insert), `b'D'` (delete),
/// `b'C'` (clear the graph: `DEFAULT` without a graph, else the named
/// graph) or `b'N'` (clear every named graph); `graph` is the named-graph
/// IRI (`None` ⇒ default graph); `triple` is the `s p o .` text
/// ([`CLEAR_BODY`] for the clears).
#[derive(Debug, Clone)]
struct RedoRec {
    op: u8,
    graph: Option<String>,
    triple: String,
}

/// The body of a clear (`C` / `N`) redo record. Readers older than clear
/// records parse every record's body as triples and refuse any op but `I` /
/// `D` for them, so this triple makes them fail loudly ("unknown redo.log op
/// 'C'") instead of skipping the clear and bringing the cleared data back
/// (review gap-doc-claims#1). This reader takes it, and the empty body the
/// clears were written with before.
const CLEAR_BODY: &str = "<urn:x-gstore:redo> <urn:x-gstore:redo#op> \"clear\" .";

/// One pending update-log record: a committed UPDATE request (framed by
/// [`logs::frame_update`]).
#[derive(Debug, Clone)]
struct UpdateRec {
    millis: u128,
    changed: usize,
    sparql: String,
}

impl UpdateRec {
    fn now(sparql: &str, changed: usize) -> UpdateRec {
        UpdateRec {
            millis: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0),
            changed,
            sparql: sparql.to_owned(),
        }
    }
}

/// What a `CLEAR` redo record clears (see [`RedoRec`]).
#[derive(Debug, Clone, Copy)]
enum RedoClear {
    Default,
    Named(u32),
    AllNamed,
}

/// An RDF database: a dictionary, the six-way triple index, and a VS-tree.
#[derive(Debug)]
pub struct Database {
    name: String,
    dict: Dictionary,
    /// The live default-graph store, selected at construction: in-memory
    /// ([`Backend::Memory`], the default), the on-disk B+ trees of a
    /// Stream-mode database ([`Backend::Disk`]) or persistent RocksDB
    /// ([`Backend::Rocks`], feature `rocksdb`). All reads/writes go through the
    /// [`TripleSource`]/[`MutableStore`] seam, so the engine is backend-agnostic.
    store: Backend,
    /// Named graphs: graph-IRI entity id → its triple store. The default graph
    /// is `store`; `GRAPH` patterns and quad updates target this map. Named
    /// graphs stay in-memory (DESIGN §"Phase 2").
    named: NamedGraphs,
    /// Signature index for query-time candidate pruning.
    vstree: VsTree,
    /// Whether `vstree` is consistent with `store`. Cleared on update; the
    /// VS-tree is only used for filtering while valid, and rebuilt on `save`
    /// or `rebuild_index`. (A stale tree is never used, preserving correctness.)
    index_valid: bool,
    /// Active transaction's undo log (`None` ⇒ auto-commit mode). Each entry is
    /// the inverse of a triple mutation, applied in reverse on rollback. A
    /// Stream-mode database records only its named-graph changes here: its
    /// default graph's transaction is the on-disk store's overlay.
    txn: Option<Vec<UndoOp>>,
    /// A [`rebuild_index`](Self::rebuild_index) is owed, run by
    /// [`run_deferred_rebuild`](Self::run_deferred_rebuild) once no
    /// transaction is open: Stream mode, it was called inside a transaction;
    /// any other store, the tree in use went stale (a clear of the default
    /// graph, reasoning; see [`mark_vstree_stale`](Self::mark_vstree_stale)).
    vstree_rebuild_pending: bool,
    /// Byte-bounded LRU cache of read-query results keyed by the SPARQL
    /// string (gStore `QueryCache`; see the `query_cache` module), cleared on
    /// any store mutation. Behind a lock so concurrent `&self` reads
    /// ([`read_as`](Self::read_as)) can look up and populate it; its entries
    /// are shared (`Arc`), so a hit is copied out after the lock is released.
    query_cache: Mutex<QueryCache>,
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
    /// When set, every committed SPARQL UPDATE is appended to this file
    /// (gStore's `update.log`), so the write history can be replayed or
    /// audited. `None` disables logging (the default).
    update_log: Option<PathBuf>,
    /// The update-log records of the open transaction's statements:
    /// appended at its commit, dropped when it rolls back.
    update_pending: Vec<UpdateRec>,
    /// The update log's appender (see [`logs`]); behind a mutex because the
    /// commit points [`sync`](Self::sync) takes `&self`.
    update_writer: Mutex<logs::LogWriter>,
    /// When set, every *committed* triple mutation is appended to this file as a
    /// redo record so it can be re-applied on recovery. `None` disables it.
    redo_log: Option<PathBuf>,
    /// Redo records of mutations not committed yet: the open (single-writer)
    /// transaction's, appended at its commit and dropped when it rolls back,
    /// or the running auto-commit statement's (see
    /// [`redo_hold`](Self::redo_hold)). Records of committed mutations whose
    /// append failed are owed by [`redo_writer`](Self::redo_writer) instead,
    /// so nothing that rolls back or disables the log drops them (review
    /// db#2).
    redo_pending: Vec<RedoRec>,
    /// The redo log's appender (see [`logs`]); behind a mutex because the
    /// backups and [`sync`](Self::sync) take `&self`.
    redo_writer: Mutex<logs::LogWriter>,
    /// An auto-commit statement (an UPDATE request, a reasoning run) or a
    /// direct-API batch call ([`redo_batch`](Self::redo_batch)) is running:
    /// its redo records are appended together at its end — a statement's
    /// after its commit point — instead of one by one as a single
    /// direct-API mutation's.
    redo_hold: bool,
    /// Incremental-backup checkpoint: the redo-log byte offset the most recent
    /// full ([`backup`](Self::backup)) or incremental
    /// ([`backup_incremental`](Self::backup_incremental)) backup ended at. The
    /// next incremental captures redo-log records from here onward, so
    /// successive incrementals chain contiguously. Atomic because both backup
    /// methods take `&self` (they run under [`persist_lock`](Self::persist_lock)).
    backup_offset: AtomicU64,
    /// Why the redo and update logs may not match the database since a
    /// commit point failed with an outcome nobody can tell (see
    /// [`CommitFate::InDoubt`]): [`backup_incremental`](Self::backup_incremental)
    /// refuses until a full [`backup`](Self::backup) clears it. Session
    /// state, like [`backup_offset`](Self::backup_offset): a reopened
    /// database starts a new chain with a full backup anyway.
    logs_in_doubt: Mutex<Option<String>>,
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
    /// [`DiskStore`](crate::kvstore::DiskStore). Behind a lock so
    /// `save(&self)` / `sync(&self)` can commit it.
    disk: Mutex<Option<persist::DiskSink>>,
    /// The first persistence failure (RocksDB dictionary / named-graph write)
    /// since the last commit point, reported by the next one.
    persist_error: Mutex<Option<String>>,
    /// Serializes the `&self` persistence entry points — [`save`](Self::save),
    /// [`checkpoint`](Self::checkpoint), [`sync`](Self::sync),
    /// [`backup`](Self::backup) and
    /// [`backup_incremental`](Self::backup_incremental) — which may now run
    /// concurrently with each other and with reads. Lock order (C8):
    /// `persist_lock` → `disk` → `persist_error` → the pager. Each entry point
    /// takes it once and delegates to internals that never take it again.
    persist_lock: Mutex<()>,
    /// Which sources SPARQL `LOAD` may read (see [`LoadPolicy`]). Permissive by
    /// default; network-facing servers install [`LoadPolicy::restricted`].
    load_policy: LoadPolicy,
    /// Advances on every applied data mutation (see
    /// [`mutation_count`](Self::mutation_count)).
    mutations: u64,
    /// Default-graph deletes applied since the VS-tree was last built, while
    /// it stayed valid (see [`vs_shrink_debt`](Self::vs_shrink_debt)).
    vs_shrink_debt: u64,
    /// When deleted terms' ids are freed (see the `reclaim` module).
    id_reclaim: IdReclaim,
    /// Reclamation candidates of the running statement, or of the open
    /// transaction (freed at its commit).
    reclaim: Candidates,
    /// Candidates whose free waits: Stream mode, for the dictionary's pins
    /// (a clone sharing the on-disk dictionary is alive); any layout, after
    /// a read error stopped their check (see [`Deferred`]).
    reclaim_deferred: Deferred,
    /// The per-predicate CSR of the default graph used by the advanced graph
    /// functions (upstream `GeneralEvaluation::csr`), built lazily and keyed
    /// by [`mutations`](Self::mutations) so any data change invalidates it
    /// (see [`csr`](Self::csr)).
    csr_cache: CsrCache,
    /// The dictionary substring index the evaluator prunes `CONTAINS` /
    /// `STRSTARTS` / `STRENDS` / plain `REGEX` candidates with, built on
    /// first use and kept for the dictionary [`generation`](Dictionary::generation)
    /// it was built from (see [`string_index`](Self::string_index)); a
    /// change that adds or frees a term drops it. In-memory dictionaries only.
    string_index: StringIndexCache,
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

/// The cached dictionary substring index of a [`Database`] (see
/// [`Database::string_index`]). Its lock is held only to look at or swap
/// the entry, never while an index is built or refreshed. Its `Debug` shows
/// only the generation and the index size.
#[derive(Default)]
struct StringIndexCache(Mutex<StringIndexSlot>);

#[derive(Default)]
struct StringIndexSlot {
    /// The index and what it was built from (absent while one is built or
    /// refreshed, and before the first).
    entry: Option<CachedStringIndex>,
    /// A build or refresh is running (with the lock released): other
    /// readers go on without the prefilter meanwhile (still exact).
    refreshing: bool,
    /// Full builds and incremental refreshes so far (see
    /// [`Database::string_index_builds`]).
    builds: (u64, u64),
}

/// A cached substring index, current for dictionary `generation`.
struct CachedStringIndex {
    generation: u64,
    index: Arc<StringIndex>,
    /// The dictionary as it was then: an O(1) copy-on-write clone (an
    /// in-memory dictionary's clone takes no pin) that a refresh compares
    /// the current one with (see `refresh_string_index`).
    base: Dictionary,
    /// Terms posted since the index was last built from scratch: each is a
    /// new term, or a freed id reused, whose old postings stay.
    posted: usize,
}

impl StringIndexCache {
    fn lock(&self) -> MutexGuard<'_, StringIndexSlot> {
        // A plain value; a panicking build left the entry out (it is rebuilt).
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl std::fmt::Debug for StringIndexCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0.try_lock().as_deref() {
            Ok(StringIndexSlot { entry: Some(e), .. }) => write!(
                f,
                "StringIndex(generation {}, {} terms)",
                e.generation,
                e.index.len()
            ),
            Ok(StringIndexSlot {
                refreshing: true, ..
            }) => f.write_str("<building>"),
            Ok(_) => f.write_str("None"),
            Err(_) => f.write_str("<locked>"),
        }
    }
}

/// The fewest terms a cached substring index may post since its last full
/// build (see [`CachedStringIndex::posted`]) before a refresh rebuilds it
/// from scratch instead of extending it — or a quarter of the dictionary,
/// if more: it bounds the stale postings freed terms leave behind.
const STRING_INDEX_STALE_MIN: usize = 4096;

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

/// Why [`Database::commit_checked`] did not commit cleanly — what became of
/// the transaction, so that a caller (a server answering `/commit`) can say
/// truthfully whether its writes stand. [`Database::commit`] reports the
/// same failures as a plain error.
#[derive(Debug)]
pub enum CommitFailure {
    /// Nothing happened: there is no transaction, or the commit was refused
    /// before anything was taken (a Stream-mode commit from inside one of
    /// the store's own snapshot reads) and the transaction is still open.
    NotCommitted(GStoreError),
    /// The commit point failed and the transaction was rolled back: none of
    /// its writes is visible any more, and none becomes durable later. On
    /// an on-disk database whose store the failure broke
    /// ([`Database::storage_failure`]) every later commit point still fails
    /// until the database is reopened, which brings back its last committed
    /// state.
    RolledBack(GStoreError),
    /// The commit point failed after the transaction could no longer be
    /// undone: its writes stay applied (visible) but are not known to be
    /// durable. On an on-disk database the store is broken (reopen it: it
    /// then holds what its write-ahead log committed — the transaction, or
    /// the part of a transaction larger than the page cache that was
    /// written before the failure); on RocksDB either the write-ahead log
    /// could not be synced, or the journal of a commit that failed could not
    /// be replayed yet (writes are refused until it is, and the next open
    /// replays it: the transaction is then undone). Its redo and
    /// update-log records follow what a reopen keeps: appended when the
    /// store's write-ahead log holds the whole transaction (the next open
    /// replays it), else left out with the logs marked in doubt —
    /// [`Database::backup_incremental`] then refuses until a full
    /// [`Database::backup`].
    NotDurable(GStoreError),
    /// The transaction is committed and durable, but a step after its
    /// commit point failed (its records could not be appended to the redo
    /// log, [`Database::enable_redo_log`]).
    CommittedWithError(GStoreError),
}

impl CommitFailure {
    /// The underlying error.
    pub fn error(&self) -> &GStoreError {
        match self {
            CommitFailure::NotCommitted(e)
            | CommitFailure::RolledBack(e)
            | CommitFailure::NotDurable(e)
            | CommitFailure::CommittedWithError(e) => e,
        }
    }

    /// The underlying error, by value.
    pub fn into_error(self) -> GStoreError {
        match self {
            CommitFailure::NotCommitted(e)
            | CommitFailure::RolledBack(e)
            | CommitFailure::NotDurable(e)
            | CommitFailure::CommittedWithError(e) => e,
        }
    }
}

/// The on-disk store's write-ahead-log counts at one moment
/// ([`Database::wal_mark`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WalMark {
    /// Flushes that named their write-ahead log.
    wal: u64,
    /// Flushes whose log's name was made durable.
    durable: u64,
}

/// What a commit point left of the commit it closed — what the redo and
/// update logs record (round-3 review: they follow what a reopen keeps).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitFate {
    /// The commit point succeeded: the records are appended.
    Durable,
    /// It failed before anything of the commit reached the store: the
    /// records are dropped (a transaction is rolled back).
    NotApplied,
    /// It failed after the commit's whole batch got a durable
    /// write-ahead-log name: the next open replays it, so the records are
    /// appended.
    Replayed,
    /// Anything else: a part (or all) of the commit may survive a reopen.
    /// The records are dropped and the logs marked in doubt, so
    /// [`Database::backup_incremental`] refuses until a full backup.
    InDoubt,
}

impl std::fmt::Display for CommitFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommitFailure::NotCommitted(e) => write!(f, "not committed: {e}"),
            CommitFailure::RolledBack(e) => {
                write!(
                    f,
                    "the commit failed and the transaction was rolled back: {e}"
                )
            }
            CommitFailure::NotDurable(e) => write!(
                f,
                "the commit failed; the transaction stays applied but is not durable: {e}"
            ),
            CommitFailure::CommittedWithError(e) => {
                write!(f, "the transaction is committed, but: {e}")
            }
        }
    }
}

/// Which journal step undoes a transaction's RocksDB writes (see
/// [`Database::undo_transaction`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RocksUndo {
    /// A rollback: the journal is still recording.
    Rollback,
    /// A commit whose journal deletion failed: the journal is stale and
    /// must be replayed instead (`RocksStore::rollback_uncleared_journal`).
    FailedCommit,
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

/// How many triples [`Database::transfer_graph`] inserts into the
/// destination per chunk (one insert batch on an in-memory default graph).
const TRANSFER_CHUNK: usize = 1 << 16;

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
            named: NamedGraphs::new(),
            vstree: VsTree::new(),
            index_valid: true, // empty store ⇔ empty tree, trivially consistent
            txn: None,
            vstree_rebuild_pending: false,
            query_cache: Mutex::new(QueryCache::new(DEFAULT_QUERY_CACHE_BYTES)),
            integer_division: IntegerDivision::default(),
            topk: TopKOptions::default(),
            gfun_max_output_bytes: crate::analytics::gfun::paths::MAX_OUTPUT_BYTES,
            pfn: None,
            pfn_limits: crate::pfn::PfnLimits::default(),
            update_log: None,
            update_pending: Vec::new(),
            update_writer: Mutex::new(logs::LogWriter::default()),
            redo_log: None,
            redo_pending: Vec::new(),
            redo_writer: Mutex::new(logs::LogWriter::default()),
            redo_hold: false,
            backup_offset: AtomicU64::new(0),
            logs_in_doubt: Mutex::new(None),
            rules: crate::reason::RuleSet::new(),
            reason_home: crate::reason::store::RuleHome::default(),
            functions: FunctionRegistry::new(),
            home: None,
            disk: Mutex::new(None),
            persist_error: Mutex::new(None),
            persist_lock: Mutex::new(()),
            load_policy: LoadPolicy::default(),
            mutations: 0,
            vs_shrink_debt: 0,
            id_reclaim: reclaim::default_id_reclaim(),
            reclaim: Candidates::default(),
            reclaim_deferred: Deferred::default(),
            csr_cache: CsrCache::new(),
            string_index: StringIndexCache::default(),
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
    ///
    /// A Stream-mode database whose on-disk store is broken (C5) refuses,
    /// also a CSR cached before the failure (it may hold a failed
    /// transaction's triples). The build cannot be cancelled: a request
    /// uses [`csr_cancellable`](Self::csr_cancellable).
    pub fn csr(&self) -> Result<std::sync::Arc<CsrGraph>> {
        self.csr_cancellable(None)
    }

    /// [`csr`](Self::csr) for a request with a cancel flag (review conc#5):
    /// a raised `cancel` stops the build at its next check — between two
    /// predicates, every [`POLL_PAIRS`](crate::analytics::csr::POLL_PAIRS)
    /// triples of an in-memory one, between the build's phases — or ends a
    /// wait for another request's build, with [`GStoreError::Cancelled`],
    /// and nothing is cached. The store is read one predicate at a time
    /// (an in-memory store in place), keeping only entity→entity edges:
    /// literal triples are never all held at once (review analytics#6). The
    /// cache's lock is not held during the build, so a reader that only
    /// asks whether a CSR is cached ([`cached_csr`](Self::cached_csr)) never
    /// waits for one.
    pub fn csr_cancellable(&self, cancel: Option<&AtomicBool>) -> Result<std::sync::Arc<CsrGraph>> {
        self.check_store_readable()?;
        self.csr_cache
            .get_or_build_cancellable(self.mutations, cancel, || {
                // A read error while building fails the build (nothing is
                // cached).
                let scope = ReadScope::enter();
                let graph = match self.store.as_memory() {
                    Some(mem) => CsrGraph::build_from_memory(mem, cancel),
                    None => {
                        let never = AtomicBool::new(false);
                        let preds = crate::dict::Space::Predicate.first_id()
                            ..self.dict.id_limit(crate::dict::Space::Predicate);
                        CsrGraph::build_cancellable(&self.store, preds, cancel.unwrap_or(&never))
                    }
                };
                match scope.take_error() {
                    Some(e) => Err(read_failure("building the CSR", e)),
                    None => graph,
                }
            })
    }

    /// Build the CSR now instead of on the first graph-function query —
    /// upstream's `loadcsr` operation and `load` with `csr=1` / `gserver -c 1`.
    pub fn preload_csr(&self) -> Result<()> {
        self.csr().map(|_| ())
    }

    /// [`preload_csr`](Self::preload_csr) for a request with a cancel flag
    /// (see [`csr_cancellable`](Self::csr_cancellable)): a raised `cancel`
    /// stops the build at its next check with [`GStoreError::Cancelled`],
    /// and nothing is cached.
    pub fn preload_csr_cancellable(&self, cancel: Option<&AtomicBool>) -> Result<()> {
        self.csr_cancellable(cancel).map(|_| ())
    }

    /// Whether a CSR for the current data is cached (no rebuild needed).
    pub fn csr_is_cached(&self) -> bool {
        self.check_store_readable().is_ok() && self.csr_cache.peek(self.mutations).is_some()
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
        self.check_store_readable().ok()?;
        self.csr_cache.peek(self.mutations)
    }

    /// Record an applied data mutation: invalidate cached query results,
    /// drop the now stale CSR (so it is not kept alive until the next graph
    /// function query) and advance [`mutation_count`](Self::mutation_count).
    fn touched(&mut self) {
        self.query_cache_mut().clear();
        self.csr_cache.clear();
        self.mutations += 1;
        // The substring index covers the dictionary: kept across a change
        // that adds or frees terms (the next substring query extends it,
        // see `string_index_cancellable`) — unless the dictionary shrank by
        // more than a quarter: a rebuild is due then, and the entry's old
        // dictionary state would keep the freed strings alive meanwhile.
        let generation = self.dict.generation();
        let live = self.dict.entity_num() + self.dict.literal_num();
        let slot = self
            .string_index
            .0
            .get_mut()
            .unwrap_or_else(|e| e.into_inner());
        if slot
            .entry
            .as_ref()
            .is_some_and(|e| e.generation != generation && live < e.index.len() - e.index.len() / 4)
        {
            slot.entry = None;
        }
    }

    /// The dictionary substring index for the current dictionary (see
    /// [`dictionary_string_index`](crate::query::engine::dictionary_string_index)):
    /// built on first use and cached, so a
    /// second query with a substring `FILTER` does not rebuild it; a change
    /// that adds or frees terms makes the next use extend it (see
    /// [`string_index_cancellable`](Self::string_index_cancellable)). `None`
    /// for an out-of-core or a small dictionary, where substring filters
    /// run without the prefilter (still exact). The build cannot be
    /// cancelled: a request uses `string_index_cancellable`.
    pub fn string_index(&self) -> Option<Arc<StringIndex>> {
        self.string_index_cancellable(None)
    }

    /// [`string_index`](Self::string_index) for a request with a cancel
    /// flag. The index is built, or refreshed, with the cache's lock
    /// released: a reader that finds a build running goes on without the
    /// prefilter (exact all the same) instead of waiting for it, and a
    /// raised `cancel` stops the build at its next check (every thousand
    /// terms or so) — nothing new is
    /// cached then (review conc#5). After a change that added or freed
    /// terms, the cached index is extended with the terms that are new or
    /// changed (one identity check per term, trigram work only for what
    /// changed) instead of being rebuilt from scratch; a full rebuild comes
    /// only once more than a quarter of the dictionary (at least
    /// 4,096 terms) was posted that way (review db#5).
    pub fn string_index_cancellable(
        &self,
        cancel: Option<&AtomicBool>,
    ) -> Option<Arc<StringIndex>> {
        if self.dict.is_disk_backed() {
            return None;
        }
        let generation = self.dict.generation();
        let stale = {
            let mut slot = self.string_index.lock();
            if let Some(e) = &slot.entry {
                if e.generation == generation {
                    return Some(Arc::clone(&e.index));
                }
            }
            if slot.refreshing {
                return None;
            }
            slot.refreshing = true;
            slot.entry.take()
        };
        /// Ends the build — also when it panics.
        struct Refreshing<'a>(&'a StringIndexCache);
        impl Drop for Refreshing<'_> {
            fn drop(&mut self) {
                self.0.lock().refreshing = false;
            }
        }
        let _refreshing = Refreshing(&self.string_index);
        let live = self.dict.entity_num() + self.dict.literal_num();
        let limit = (live / 4).max(STRING_INDEX_STALE_MIN);
        if let Some(e) = stale.filter(|e| e.posted <= limit) {
            // Extend it in place (a copy if someone still holds it).
            let mut index = Arc::try_unwrap(e.index).unwrap_or_else(|shared| (*shared).clone());
            let mut posted = e.posted;
            match crate::query::engine::refresh_string_index(
                &mut index,
                &e.base,
                &self.dict,
                &mut posted,
                limit,
                cancel,
            ) {
                Ok(true) => {
                    let index = Arc::new(index);
                    let mut slot = self.string_index.lock();
                    slot.builds.1 += 1;
                    slot.entry = Some(CachedStringIndex {
                        generation,
                        index: Arc::clone(&index),
                        base: self.dict.clone(),
                        posted,
                    });
                    return Some(index);
                }
                // Too much changed: build it from scratch below.
                Ok(false) => {}
                // Cancelled: keep what there was (with the part of the
                // change posted, a superset still for its dictionary state).
                Err(_) => {
                    self.string_index.lock().entry = Some(CachedStringIndex {
                        index: Arc::new(index),
                        posted,
                        ..e
                    });
                    return None;
                }
            }
        }
        let index = dictionary_string_index_cancellable(&self.dict, cancel)
            .ok()
            .flatten()?;
        let mut slot = self.string_index.lock();
        slot.builds.0 += 1;
        slot.entry = Some(CachedStringIndex {
            generation,
            index: Arc::clone(&index),
            base: self.dict.clone(),
            posted: 0,
        });
        Some(index)
    }

    /// The cached substring index, if one is cached for the current
    /// dictionary (never builds). For tests and monitoring.
    pub fn cached_string_index(&self) -> Option<Arc<StringIndex>> {
        let generation = self.dict.generation();
        let slot = self.string_index.lock();
        slot.entry
            .as_ref()
            .filter(|e| e.generation == generation)
            .map(|e| Arc::clone(&e.index))
    }

    /// How many times this database built its substring index from scratch
    /// and how many times it extended it (`(full, incremental)`). For tests
    /// and diagnostics.
    #[doc(hidden)]
    pub fn string_index_builds(&self) -> (u64, u64) {
        self.string_index.lock().builds
    }

    /// The query cache, through its lock. A lock poisoned by a panic is
    /// recovered with the cache emptied (it only holds copies of answers).
    fn query_cache(&self) -> MutexGuard<'_, QueryCache> {
        match self.query_cache.lock() {
            Ok(cache) => cache,
            Err(poisoned) => {
                self.query_cache.clear_poison();
                let mut cache = poisoned.into_inner();
                cache.clear();
                cache
            }
        }
    }

    /// The query cache, exclusively (no locking needed).
    fn query_cache_mut(&mut self) -> &mut QueryCache {
        let poisoned = self.query_cache.is_poisoned();
        self.query_cache.clear_poison();
        let cache = self
            .query_cache
            .get_mut()
            .unwrap_or_else(|e| e.into_inner());
        if poisoned {
            cache.clear();
        }
        cache
    }

    /// Serialize a `&self` persistence entry point (see
    /// [`persist_lock`](Self::persist_lock)); it guards no data, so a poisoned
    /// lock is simply taken over.
    fn persist_guard(&self) -> MutexGuard<'_, ()> {
        self.persist_lock.lock().unwrap_or_else(|e| {
            self.persist_lock.clear_poison();
            e.into_inner()
        })
    }

    /// How many default-graph deletes (including a rollback's removal of the
    /// transaction's inserts) were applied while the VS-tree stayed valid
    /// since it was last built. Deletes leave the tree in place — an entity's
    /// signature only loses bits, so its stored signature stays a sound
    /// superset — at the price of some pruning power; this counts that debt.
    /// [`rebuild_index`](Self::rebuild_index) clears it.
    pub fn vs_shrink_debt(&self) -> u64 {
        self.vs_shrink_debt
    }

    /// Rebuild the VS-tree from the current store and mark the index valid.
    /// A Stream-mode database rewrites its `vstree.kv` (see
    /// [`try_rebuild_index`](Self::try_rebuild_index)); a failure there is
    /// logged and leaves the index unused.
    pub fn rebuild_index(&mut self) {
        if let Err(e) = self.try_rebuild_index() {
            crate::gs_warn!("rebuilding the VS-tree of '{}' failed: {e}", self.name);
        }
    }

    /// [`rebuild_index`](Self::rebuild_index), reporting a failure. A
    /// Stream-mode database signs every entity in one streaming pass over its
    /// committed store ([`DiskStore::for_each_entity_adjacency`](crate::kvstore::DiskStore::for_each_entity_adjacency)),
    /// writes `vstree.kv` beside it (built under a temporary name, then renamed
    /// over the old one), marks it fresh in the store's header and makes that
    /// durable; inside a transaction the rebuild is deferred until the
    /// transaction ends. Any other database builds its in-memory tree.
    pub fn try_rebuild_index(&mut self) -> Result<()> {
        if self.store.as_disk().is_none() {
            // A RocksDB store can fail a read: the old tree is dropped then
            // and none is used until a rebuild succeeds.
            let entries = checked_vstree_entries(&self.store).inspect_err(|_| {
                self.index_valid = false;
                self.vstree = VsTree::new();
            })?;
            self.vstree = VsTree::build(entries);
            self.index_valid = true;
            self.vs_shrink_debt = 0;
            return Ok(());
        }
        if self.txn.is_some() {
            self.vstree_rebuild_pending = true;
            return Ok(());
        }
        self.vstree_rebuild_pending = false;
        let Some(home) = self.home.clone() else {
            return Err(GStoreError::Database(
                "an on-disk database without a home directory cannot rebuild vstree.kv".into(),
            ));
        };
        // Never filter through the old tree again, whatever happens below.
        self.index_valid = false;
        self.vstree = VsTree::new();
        let Some(dt) = self.store.as_disk_mut() else {
            return Ok(());
        };
        // The pages the tree is signed from must be durable before it
        // replaces `vstree.kv`: direct writes not yet at a commit point sit
        // in the page cache only, and a crash after the rename would leave
        // a tree of data the store does not hold (deleted edges missing from
        // it while the store still has them — not a superset, C9) under a
        // header that says the tree is fresh.
        dt.commit_point()?;
        let entries = disk_vstree_entries(dt.disk_store()?)?;
        self.vstree = persist::write_vstree_kv(&home.join(VSTREE_KV_FILE), entries)?;
        dt.mark_vstree_fresh();
        dt.commit_point()?;
        self.index_valid = true;
        self.vs_shrink_debt = 0;
        self.query_cache_mut().clear();
        Ok(())
    }

    /// Run a [`rebuild_index`](Self::rebuild_index) that is owed
    /// ([`vstree_rebuild_pending`](Self::vstree_rebuild_pending)) once no
    /// transaction is open: at the end of the transaction, or of the
    /// operation that made the tree stale outside one. A rebuild that fails
    /// is logged and leaves the index unused (it is not retried).
    fn run_deferred_rebuild(&mut self) {
        if self.vstree_rebuild_pending && self.txn.is_none() {
            self.rebuild_index();
            self.vstree_rebuild_pending = false;
        }
    }

    /// The VS-tree no longer matches the default graph (a clear, or
    /// reasoning a disk-backed tree cannot take): stop filtering through it.
    /// A tree that was in use on a store other than Stream mode's is then
    /// owed a rebuild, which [`run_deferred_rebuild`](Self::run_deferred_rebuild)
    /// runs at the end of the operation (or transaction) — so it is not
    /// lost for the rest of the session (review db#4). Rebuilding right
    /// after a clear costs what is left, usually nothing; a Stream-mode
    /// `vstree.kv` is rewritten only by an explicit
    /// [`rebuild_index`](Self::rebuild_index) or at the next open (its
    /// rebuild takes commit points, which an operation must not take
    /// part-way).
    fn mark_vstree_stale(&mut self) {
        if self.index_valid && self.store.as_disk().is_none() {
            self.vstree_rebuild_pending = true;
        }
        self.index_valid = false;
    }

    /// Whether queries may filter through the VS-tree now: it is consistent
    /// with the store ([`index_valid`](Self::index_valid)) and, on a
    /// Stream-mode database, no open transaction adds triples the tree (built
    /// from committed data) lacks (C9; `DiskTriples::vstree_stale`).
    fn vstree_usable(&self) -> bool {
        self.index_valid && self.store.as_disk().is_none_or(|dt| !dt.transaction_adds())
    }

    /// Fail a statement up front when the on-disk store of a Stream-mode
    /// database is broken by an earlier write error (C5): it must be reopened.
    /// (Reads report the same error through the read scope; this also covers
    /// a statement that would read nothing from the store.)
    fn check_store_readable(&self) -> Result<()> {
        match self.store.as_disk().and_then(|dt| dt.broken()) {
            Some(e) => Err(GStoreError::Database(format!(
                "the on-disk store is broken by an earlier write error ({e}); reopen the database"
            ))),
            None => Ok(()),
        }
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
        // Signed first, in a read scope: a read error fails the call and
        // leaves the current tree (and its file) as they were.
        let entries = checked_vstree_entries(&self.store)?;
        // Fresh build: drop any prior node file (and its WAL) at this path.
        remove_file_and_wal(path)?;
        self.vstree = VsTree::build_disk(path, entries)?;
        self.index_valid = true;
        self.vs_shrink_debt = 0;
        self.query_cache_mut().clear();
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
        self.query_cache_mut().clear();
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

    /// Build a database by importing RDF sources: files (format by extension —
    /// N-Triples/Turtle/N-Quads/TriG/RDF-XML/JSON-LD/RDFa, unknown → Turtle;
    /// see [`import::RdfFormat`]), directories (every RDF file below them,
    /// sorted) and `http(s)://` URLs. Streaming formats are read with bounded
    /// memory; N-Quads / TriG statements keep their named graph; every source
    /// gets its own blank-node scope. Strict: the first statement that fails
    /// is the error (see [`build_from_sources`](Self::build_from_sources) for
    /// a tolerant build).
    pub fn build_from_files<P: AsRef<Path>>(
        name: impl Into<String>,
        files: &[P],
    ) -> Result<Database> {
        let sources = import::sources_from_paths(files)?;
        let (db, _) = Database::build_from_sources(
            name,
            &sources,
            &import::ImportOptions::strict(),
            &mut (),
        )?;
        Ok(db)
    }

    /// Build a database from `sources` with explicit [`import::ImportOptions`]
    /// (strict or tolerant, blank-node scoping, limits), reporting skipped
    /// statements to `log` (e.g. an [`import::ParseErrorLog`]). Quads are
    /// encoded batch by batch as they stream in; the default graph is indexed
    /// once at the end. Returns the database and the import counts.
    pub fn build_from_sources(
        name: impl Into<String>,
        sources: &[import::Source],
        opts: &import::ImportOptions,
        log: &mut dyn import::ErrorLog,
    ) -> Result<(Database, import::ImportReport)> {
        let mut db = Database::new(name);
        let mut acc = BuildAccumulator::default();
        let report =
            import::import_sources(sources, "build", opts, log, &mut |batch: &mut Vec<Quad>| {
                acc.add(&mut db, batch)
            })?;
        acc.finish(&mut db);
        Ok((db, report))
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
    /// Parsing and dictionary encoding overlap (the document streams in
    /// batches). Strict; blank-node labels are kept as written.
    pub fn build_from_str_with_progress<F: FnMut(BuildProgress)>(
        name: impl Into<String>,
        content: &str,
        mut progress: F,
    ) -> Result<Database> {
        let mut db = Database::new(name);
        progress(BuildProgress::RdfParse);
        let opts = import::ImportOptions {
            blank_nodes: import::BlankNodes::Keep,
            ..import::ImportOptions::strict()
        };
        let mut acc = BuildAccumulator::default();
        let mut announced = false;
        import::import_reader(
            content.as_bytes(),
            import::RdfFormat::Turtle,
            "<string>",
            &opts,
            &mut (),
            &mut |batch: &mut Vec<Quad>| {
                if !announced {
                    progress(BuildProgress::Dictionary);
                    announced = true;
                }
                acc.add(&mut db, batch)
            },
        )?;
        if !announced {
            progress(BuildProgress::Dictionary);
        }
        progress(BuildProgress::Index);
        acc.finish(&mut db);
        progress(BuildProgress::Done);
        Ok(db)
    }

    /// Insert the statements of RDF `sources` into this database (`gadd`,
    /// `/api` `batchInsert`): like [`build_from_sources`](Self::build_from_sources)
    /// but through the ordinary insert path (transaction log, redo log,
    /// write-through to a persistent layout, incremental index). N-Quads /
    /// TriG statements go to their named graph. `applied` counts the
    /// statements that were new (duplicates are not), and stays accurate when
    /// an error stops the import part-way.
    pub fn insert_sources(
        &mut self,
        sources: &[import::Source],
        opts: &import::ImportOptions,
        log: &mut dyn import::ErrorLog,
        applied: &mut u64,
    ) -> Result<import::ImportReport> {
        // One redo-log commit for the whole call (review db#8).
        self.redo_batch(|this| {
            // RocksDB: one journaled statement — a crash undoes all of it,
            // never a prefix; an error keeps what was applied (see `applied`).
            let journaled = this.rocks_begin_statement()?;
            let report = this.rocks_statement_body(journaled, |db| {
                import::import_sources(
                    sources,
                    "batch insert",
                    opts,
                    log,
                    &mut |batch: &mut Vec<Quad>| {
                        let mut added = Vec::new();
                        let quads = batch.drain(..).map(|q| {
                            let graph = graph_iri(&q.graph);
                            (q.into_triple(), graph)
                        });
                        let result = db.insert_statements(quads, &mut added);
                        *applied += added.len() as u64;
                        result
                    },
                )
            });
            this.end_sources_statement(journaled, report)
        })
    }

    /// The end of an `insert_sources` / `remove_sources` statement on
    /// RocksDB ([`rocks_end_statement`](Self::rocks_end_statement)); the
    /// import's own error comes first (a failed journal deletion also fails
    /// the next commit point).
    fn end_sources_statement(
        &mut self,
        journaled: bool,
        report: Result<import::ImportReport>,
    ) -> Result<import::ImportReport> {
        if !journaled {
            return report;
        }
        let ended = self.rocks_end_statement();
        let report = report?;
        ended.map(|()| report)
    }

    /// Remove the statements of RDF `sources` from this database (`gsub`,
    /// `/api` `batchRemove`); `applied` counts the statements that existed.
    /// Pass options [`for_removal`](import::ImportOptions::for_removal) so
    /// blank-node labels are read verbatim: a statement can only name a
    /// stored blank node by its stored label (as an export writes it).
    pub fn remove_sources(
        &mut self,
        sources: &[import::Source],
        opts: &import::ImportOptions,
        log: &mut dyn import::ErrorLog,
        applied: &mut u64,
    ) -> Result<import::ImportReport> {
        // One redo-log commit for the whole call (review db#8).
        self.redo_batch(|this| {
            // RocksDB: one journaled statement (see `insert_sources`); the
            // batches' frees wait for its end.
            let journaled = this.rocks_begin_statement()?;
            let report = this.rocks_statement_body(journaled, |db| {
                import::import_sources(
                    sources,
                    "batch remove",
                    opts,
                    log,
                    &mut |batch: &mut Vec<Quad>| {
                        // In a read scope (C2): a term that cannot be looked
                        // up must fail the removal, not read as absent.
                        let scope = ReadScope::enter();
                        let quads = batch.drain(..).map(|q| {
                            let graph = graph_iri(&q.graph);
                            (q.into_triple(), graph)
                        });
                        let (removed, err) = db.remove_statements(quads, Some(&scope));
                        *applied += removed as u64;
                        if let Some(e) = err {
                            db.flush_reclaim();
                            return Err(read_failure("removing statements", e));
                        }
                        // Each batch frees the ids it left unreferenced
                        // (RocksDB: they wait for the statement's end; only
                        // those are kept).
                        db.flush_reclaim();
                        db.keep_dead_candidates();
                        db.check_store_readable()
                    },
                )
            });
            this.end_sources_statement(journaled, report)
        })
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
    /// [`Backend::Memory`] backend — which an on-disk database opened in
    /// Materialize mode also uses for its working set (used by snapshot MVCC
    /// and tests).
    ///
    /// # Panics
    /// For a database with no in-RAM store: a Stream-mode on-disk database
    /// ([`Backend::Disk`]) or a RocksDB-backed one. Query those through
    /// [`query`](Self::query)/[`select`](Self::select) or read
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
            .filter_map(|&id| self.dict.term(id).map(TermStr::into_string))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        classes.sort();

        // Properties: predicates used in data; subjects of subProperty/domain/range.
        let mut props: HashSet<String> = self
            .store
            .predicates()
            .into_iter()
            .filter_map(|p| self.dict.pred(p).map(TermStr::into_string))
            .collect();
        for iri in [RDFS_SUBPROP, RDFS_DOMAIN, RDFS_RANGE] {
            if let Some(p) = pid(iri) {
                for &(s, _o) in &self.store.so_by_p(p) {
                    if let Some(name) = self.dict.term(s) {
                        props.insert(name.into_string());
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
        Ok(self.try_insert_triple_id(t)?.is_some())
    }

    /// [`try_insert_triple`](Self::try_insert_triple), returning the ids of a
    /// newly added triple.
    fn try_insert_triple_id(&mut self, t: &Triple) -> Result<Option<IdTriple>> {
        let Some((_, id)) = self.intern_for_insert(t, None)? else {
            return Ok(None);
        };
        Ok(self.insert_default_id(id)?.then_some(id))
    }

    /// Insert the default-graph triple `id`, whose terms are interned (and
    /// persisted) already, with its VS-tree and book-keeping; `true` if it
    /// was newly added.
    fn insert_default_id(&mut self, id: IdTriple) -> Result<bool> {
        // Stream mode: by id, into the open transaction's overlay or at once;
        // a write error breaks the store and fails this insert.
        let (changed, disk) = match self.store.as_disk_mut() {
            Some(dt) => (dt.insert_triple(id)?, true),
            None => (self.store.insert(id), false),
        };
        if changed {
            // While the index is consistent, keep it consistent *incrementally*:
            // the inserted edge only changes the signatures of its subject (and
            // its object, when that is an entity), so re-sign just those entities
            // and update their VS-tree entries in place — no full rebuild. A
            // stale index stays stale here; the operation that made an
            // in-memory tree stale rebuilds it (see `run_deferred_rebuild`).
            //
            // An out-of-core (disk-backed) tree can't be updated in place here
            // (its nodes live in a paged file), so we instead stop using it:
            // queries run without the filter until `rebuild_index` rebuilds
            // it, or a reopen rebuilds the `vstree.kv` the store's header now
            // marks stale (Materialize mode always, Stream mode with
            // `vstree_on_open`) — the non-incremental paged tree, PHASE6_PLAN
            // §8 D6 (review db#4).
            if self.index_valid {
                if self.vstree.is_disk_backed() {
                    self.index_valid = false;
                } else {
                    self.vstree_index_insert(id);
                }
            }
            self.inserted_default(id, disk);
        }
        Ok(changed)
    }

    /// The book-keeping of a newly added default-graph triple, the VS-tree
    /// aside: caches, the undo log (Stream mode: the store's overlay undoes
    /// it), the redo log and the write-through.
    fn inserted_default(&mut self, id: IdTriple, disk: bool) {
        self.touched();
        if let Some(log) = self.txn.as_mut().filter(|_| !disk) {
            log.push(UndoOp::Del(None, id)); // rollback removes what we added
        }
        self.record_redo(b'I', None, id);
        self.persist_mutation(true, None, id);
    }

    /// Insert statements (`None` graph = the default graph), as
    /// [`insert_quad_id`](Self::insert_quad_id) of each in order would,
    /// pushing each newly added one to `applied`. On an in-memory store the
    /// default-graph triples are interned first (terms before triples, as
    /// one by one) and merged into the store as one batch
    /// ([`MutableStore::insert_many`]): a new term reusing a low id lands
    /// near the front of long adjacency lists, which one insert per triple
    /// would move once per triple. Their book-keeping then runs per added
    /// triple, in input order; they are pushed to `applied` after the batch
    /// (the named-graph ones as they are applied — the graphs are
    /// independent, so only the order within one graph matters). On an
    /// error (an id space exhausted) the statements before the failing one
    /// stay inserted, as one by one, and the error is returned.
    fn insert_statements<T, G>(
        &mut self,
        items: impl IntoIterator<Item = (T, Option<G>)>,
        applied: &mut Vec<(Option<u32>, IdTriple)>,
    ) -> Result<()>
    where
        T: std::borrow::Borrow<Triple>,
        G: AsRef<str>,
    {
        let batched = self.store.as_memory().is_some();
        let mut pending: Vec<IdTriple> = Vec::new();
        let mut result = Ok(());
        for (t, g) in items {
            let (t, g) = (t.borrow(), g.as_ref().map(AsRef::as_ref));
            let step = if batched && g.is_none() {
                self.intern_for_insert(t, None)
                    .map(|r| pending.extend(r.map(|(_, id)| id)))
            } else {
                self.insert_quad_id(t, g).map(|r| applied.extend(r))
            };
            if let Err(e) = step {
                result = Err(e);
                break;
            }
        }
        if !pending.is_empty() {
            let added = self.insert_default_batch(&pending);
            applied.extend(added.into_iter().map(|id| (None, id)));
        }
        result
    }

    /// Insert the interned default-graph triples `ids` into the in-memory
    /// store as one batch and run each added triple's book-keeping, in input
    /// order. Returns the added triples. The VS-tree is kept incrementally
    /// as one by one would ([`vstree_index_insert`](Self::vstree_index_insert)),
    /// reading no edge from the store for an entity with no default-graph
    /// edge before the batch (a new term, or a reclaimed id reused for one):
    /// its exact signature is the union of its batch edges, so its entry —
    /// possibly the stale one of the term its id named before — is replaced
    /// by its first batch edge's signature and the later ones are ORed in.
    fn insert_default_batch(&mut self, ids: &[IdTriple]) -> Vec<IdTriple> {
        // Entity -> FRESH (no edge before the batch, entry not yet replaced),
        // SIGNED (fresh, entry replaced), or OLD (had edges: its entry is
        // folded into, as one by one).
        const OLD: u8 = 0;
        const FRESH: u8 = 1;
        const SIGNED: u8 = 2;
        let fold = self.index_valid && !self.vstree.is_disk_backed();
        let mut state: HashMap<u32, u8> = HashMap::new();
        if fold {
            for id in ids {
                let obj = is_entity_id(id.obj).then_some(id.obj);
                for e in std::iter::once(id.sub).chain(obj) {
                    if let std::collections::hash_map::Entry::Vacant(v) = state.entry(e) {
                        let edges = self.store.count_po_by_s(e) + self.store.count_ps_by_o(e);
                        v.insert(if edges == 0 { FRESH } else { OLD });
                    }
                }
            }
        }
        let results = self.store.insert_many(ids);
        let mut added = Vec::new();
        for (&id, new) in ids.iter().zip(results) {
            if !new {
                continue;
            }
            if self.index_valid {
                if self.vstree.is_disk_backed() {
                    self.index_valid = false;
                } else {
                    let mut out = Signature::new();
                    out.encode_edge(id.pred, id.obj, EdgeDir::Out);
                    // Literal objects are not VS-tree entities.
                    let inc = is_entity_id(id.obj).then(|| {
                        let mut inc = Signature::new();
                        inc.encode_edge(id.pred, id.sub, EdgeDir::In);
                        (id.obj, inc)
                    });
                    for (e, sig) in std::iter::once((id.sub, out)).chain(inc) {
                        if state.get(&e) == Some(&FRESH) {
                            state.insert(e, SIGNED);
                            if !self.vstree.update(e, sig) {
                                self.vstree.insert(e, sig);
                            }
                        } else if !self.vstree.union_into(e, &sig) {
                            self.upsert_entity_signature(e);
                        }
                    }
                }
            }
            self.inserted_default(id, false);
            added.push(id);
        }
        added
    }

    /// Fold a just-inserted default-graph triple into the VS-tree incrementally
    /// (called only while the index is valid). The subject — and the object when
    /// it is an entity — gained an incident edge. That edge only adds bits, so
    /// its signature is ORed into the entity's tree entry and up its path
    /// ([`VsTree::union_into`], O(height)): the entry becomes what re-signing
    /// the entity would give (a superset of it after deletes), without reading
    /// the entity's other edges — a class with a million members costs no
    /// more than a leaf entity. An entity new to the tree is signed from the
    /// store and inserted. Mirrors [`build_vstree`]'s per-entity signing, so
    /// the candidate filter remains a sound superset.
    fn vstree_index_insert(&mut self, id: IdTriple) {
        let mut out = Signature::new();
        out.encode_edge(id.pred, id.obj, EdgeDir::Out);
        // An entity whose first edge this is — new, or a reclaimed id reused
        // for a new term — gets its exact signature: the tree may still hold
        // the stale entry of the term its id named before.
        if self.is_first_edge(id.sub) || !self.vstree.union_into(id.sub, &out) {
            self.upsert_entity_signature(id.sub);
        }
        // Literal objects are not VS-tree entities (see `build_vstree`).
        if is_entity_id(id.obj) {
            let mut inc = Signature::new();
            inc.encode_edge(id.pred, id.sub, EdgeDir::In);
            if self.is_first_edge(id.obj) || !self.vstree.union_into(id.obj, &inc) {
                self.upsert_entity_signature(id.obj);
            }
        }
    }

    /// Whether entity `e` has exactly one edge in the default graph (O(1)
    /// on the in-memory store, the only one whose tree is updated in place).
    fn is_first_edge(&self, e: u32) -> bool {
        self.store.count_po_by_s(e) + self.store.count_ps_by_o(e) == 1
    }

    /// Recompute entity `e`'s full signature from its current in/out edges and
    /// update its VS-tree entry in place, inserting it if it is not yet indexed.
    fn upsert_entity_signature(&mut self, e: u32) {
        let mut sig = Signature::new();
        let (out, inc) = (self.store.po_by_s(e), self.store.ps_by_o(e));
        for &(p, o) in &out {
            sig.encode_edge(p, o, EdgeDir::Out);
        }
        for &(p, s) in &inc {
            sig.encode_edge(p, s, EdgeDir::In);
        }
        #[cfg(test)]
        RESIGNED_EDGES.with(|c| c.set(c.get() + (out.len() + inc.len()) as u64));
        if !self.vstree.update(e, sig) {
            self.vstree.insert(e, sig);
        }
    }

    /// Remove one triple. Returns `true` if it existed. Does not intern: if any
    /// term is unknown, the triple cannot exist.
    pub fn remove_triple(&mut self, t: &Triple) -> bool {
        let removed = self.remove_triple_quiet(t);
        // Outside a transaction, a delete frees the ids it left unreferenced.
        self.flush_reclaim();
        removed
    }

    /// [`remove_triple`](Self::remove_triple) without the reclamation at its
    /// end (the statement that calls it reclaims once, when it is done).
    fn remove_triple_quiet(&mut self, t: &Triple) -> bool {
        match self.known_ids(t) {
            Some(id) => self.remove_default_id(id),
            None => false,
        }
    }

    /// The ids of a default-graph triple whose terms are all known (looked
    /// up, never interned: with an unknown term the triple cannot exist).
    fn known_ids(&self, t: &Triple) -> Option<IdTriple> {
        Some(IdTriple::new(
            self.dict.entity_id(&t.subject.dict_key())?,
            self.dict.predicate_id(&t.predicate.dict_key())?,
            self.dict.term_id(&t.object)?,
        ))
    }

    /// [`remove_triple`](Self::remove_triple), failing when the removal cannot
    /// be read through (REST `/batchRemove`): it runs in a read scope (C2), so
    /// a term whose lookup failed (an out-of-core dictionary's read error) is
    /// an error rather than "no such term" — which reads as an absent
    /// triple — and so is a store broken by a write error. `Ok(true)` if the
    /// triple existed.
    pub fn try_remove_triple(&mut self, t: &Triple) -> Result<bool> {
        let scope = ReadScope::enter();
        let removed = self.remove_triple(t);
        if let Some(e) = scope.take_error() {
            return Err(read_failure("removing a triple", e));
        }
        drop(scope);
        self.check_store_readable()?;
        Ok(removed)
    }

    /// Remove the default-graph triple `id`; `true` if it existed. (Stream
    /// mode: a write error breaks the store, answers `false` and is reported
    /// by the next commit point.)
    fn remove_default_id(&mut self, id: IdTriple) -> bool {
        let disk = self.store.as_disk().is_some();
        let changed = self.store.remove(id);
        if changed {
            self.removed_default(id, disk);
        }
        changed
    }

    /// The book-keeping of a removed default-graph triple.
    fn removed_default(&mut self, id: IdTriple, disk: bool) {
        // A delete leaves the VS-tree alone, valid: the removed edge only
        // *shrinks* the subject/object signatures, so their stored entries
        // stay sound supersets (an entity left with no edge at all is a
        // harmless phantom — the joins check every candidate against the
        // store). A freed id that `reclaim_unused` later reuses is either
        // re-signed exactly by the insert that uses it, or has no
        // default-graph edge, so its entry is a superset too. This holds
        // for the read-only out-of-core tree as well. The cost is some
        // pruning power, counted in `vs_shrink_debt`.
        if self.index_valid {
            self.vs_shrink_debt += 1;
        }
        self.touched();
        if let Some(log) = self.txn.as_mut().filter(|_| !disk) {
            log.push(UndoOp::Add(None, id)); // rollback re-adds what we removed
        }
        self.record_redo(b'D', None, id);
        self.persist_mutation(false, None, id);
        self.note_removed(None, id);
    }

    /// Remove statements (`None` graph = the default graph), as
    /// [`delete_quad`](Self::delete_quad) of each in order would; returns
    /// how many existed. On an in-memory store the default-graph triples
    /// are removed as one batch ([`MutableStore::remove_many`]) after the
    /// lookups, then their book-keeping runs per removed triple in input
    /// order (see [`insert_statements`](Self::insert_statements)). With
    /// `scope`, a read error it notes stops the removal after the statement
    /// that hit it (the ones before it stay removed) and is returned. Frees
    /// nothing: the caller reclaims at the end of its statement.
    fn remove_statements<T, G>(
        &mut self,
        items: impl IntoIterator<Item = (T, Option<G>)>,
        scope: Option<&ReadScope>,
    ) -> (usize, Option<String>)
    where
        T: std::borrow::Borrow<Triple>,
        G: AsRef<str>,
    {
        let batched = self.store.as_memory().is_some();
        let mut pending: Vec<IdTriple> = Vec::new();
        let mut n = 0;
        let mut err = None;
        for (t, g) in items {
            let (t, g) = (t.borrow(), g.as_ref().map(AsRef::as_ref));
            if batched && g.is_none() {
                pending.extend(self.known_ids(t));
            } else if self.delete_quad(t, g) {
                n += 1;
            }
            if let Some(e) = scope.and_then(ReadScope::take_error) {
                err = Some(e);
                break;
            }
        }
        n += self.remove_default_batch(&pending);
        let err = err.or_else(|| scope.and_then(ReadScope::take_error));
        (n, err)
    }

    /// Remove the default-graph triples `ids` from the in-memory store as
    /// one batch and run each removed triple's book-keeping, in input order.
    /// Returns how many existed.
    fn remove_default_batch(&mut self, ids: &[IdTriple]) -> usize {
        if ids.is_empty() {
            return 0;
        }
        let results = self.store.remove_many(ids);
        let mut n = 0;
        for (&id, gone) in ids.iter().zip(results) {
            if gone {
                self.removed_default(id, false);
                n += 1;
            }
        }
        n
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
    /// open (see `crate::backend::rocks`). On a Stream-mode on-disk database
    /// its default-graph writes collect in the store's overlay (no triple
    /// reaches the pager before [`commit`](Self::commit)) and its named-graph
    /// quads are queued there too.
    pub fn begin(&mut self) -> Result<()> {
        if self.txn.is_some() {
            return Err(GStoreError::Database(
                "a transaction is already active".into(),
            ));
        }
        self.rocks_begin_journal()?;
        if let Some(dt) = self.store.as_disk_mut() {
            dt.begin()?;
        }
        self.txn = Some(Vec::new());
        // The terms this transaction interns new are freed again if it rolls
        // back (see the `reclaim` module).
        if self.id_reclaim == IdReclaim::Immediate {
            self.dict.track_allocations(true);
        }
        Ok(())
    }

    /// Commit the active transaction. Its buffered redo records (this
    /// transaction's mutations) are appended to the redo log once it is
    /// durable.
    ///
    /// On a persistent layout this is a commit point: the transaction's queued
    /// write-through reaches the store and is made durable (see [`StorageKind`]).
    /// The outcome is binary where the layout allows it: when the commit
    /// point fails, the transaction is rolled back (none of its writes stays
    /// visible or becomes durable later) unless it can no longer be undone —
    /// see [`commit_checked`](Self::commit_checked), which tells the cases
    /// apart; this method reports each of them as an error.
    pub fn commit(&mut self) -> Result<()> {
        self.commit_checked().map_err(CommitFailure::into_error)
    }

    /// [`commit`](Self::commit), reporting what became of the transaction
    /// when it did not commit cleanly ([`CommitFailure`]).
    ///
    /// The undo log is kept until the commit point succeeded: the queued
    /// write-through is applied and flushed first (RocksDB: the undo journal
    /// is deleted — that is the commit — with the terms the transaction
    /// freed in the same batch). If the commit point fails before anything
    /// of the transaction can have reached the store (an on-disk store's
    /// write-ahead log of the failed flush never got its name, and no flush
    /// the page cache made in between did either; RocksDB: the journal could
    /// not be deleted), the transaction is undone in memory — RocksDB: its
    /// journal is replayed — and [`CommitFailure::RolledBack`] is returned;
    /// otherwise its writes stay applied and [`CommitFailure::NotDurable`]
    /// says so. The ids a transaction's deletes left unreferenced are freed
    /// in the same commit point (the in-memory dictionary retires them only
    /// once it succeeded), and only a committed transaction reaches the redo
    /// log.
    pub fn commit_checked(&mut self) -> std::result::Result<(), CommitFailure> {
        if self.txn.is_none() {
            return Err(CommitFailure::NotCommitted(GStoreError::Database(
                "no active transaction".into(),
            )));
        }
        // On-disk stores: a new write-ahead log from here on means the
        // commit may survive (in part) whatever happens next.
        let start = self.wal_mark();
        // Stream mode: apply the overlay (named-graph operations, removals,
        // additions) under the store's writer gate.
        let mut overlay_failure = None;
        if let Some(dt) = self.store.as_disk_mut() {
            if let Err(e) = dt.commit() {
                if dt.in_transaction() {
                    // Refused before anything was taken (from inside one of
                    // the store's own snapshot reads): still open.
                    return Err(CommitFailure::NotCommitted(e));
                }
                // The store is broken (every read is refused until it is
                // reopened) and the overlay is gone.
                overlay_failure = Some(e);
            }
        }
        let log = self.txn.take().unwrap_or_default();
        let minted = self.dict.take_allocations();
        self.dict.track_allocations(false);
        if let Some(e) = overlay_failure {
            return Err(self.failed_commit(log, minted, e, start, None));
        }
        // Materialize: apply the queued write-through to the on-disk store.
        self.disk_commit_pending();
        // RocksDB: deleting the undo journal is what commits the
        // transaction; the terms it freed leave the `dict` CF in the same
        // atomic batch.
        let rocks = self.rocks_journaling();
        if rocks {
            let dead = self.committed_dead();
            if let Err(e) = self.rocks_commit_journal_with(&dead) {
                // Not committed: a later write would still delete the
                // journal (committing it) while a reopen would replay it
                // (undoing it). Undo it now, through the journal.
                return Err(self.undo_failed_commit(log, minted, e, RocksUndo::FailedCommit));
            }
            self.free_dead(dead, true);
        }
        // The other layouts: the transaction's frees, after its writes and
        // in the same commit point — the ids its deletes left unreferenced
        // in the committed state. A Stream-mode store frees them in its
        // pages (a failed commit point breaks it: the reopen drops them with
        // the rest); a Materialize-mode one gets them queued to its file now
        // and the in-memory dictionary retires them only once the commit
        // point succeeded, so an undo still finds every term it re-adds.
        let retire = if rocks {
            Vec::new()
        } else {
            let dead = self.committed_dead();
            if self.store.as_disk().is_some() {
                self.free_dead(dead, false);
                Vec::new()
            } else {
                self.persist_frees(dead, false)
            }
        };
        let point = if rocks {
            // The journal is gone (committed there): a failed sync leaves
            // the transaction applied with an unknown outcome.
            self.commit_point().map_err(|e| (e, CommitFate::InDoubt))
        } else {
            self.commit_point_fate(start)
        };
        if let Err((e, fate)) = point {
            return Err(self.failed_commit(log, minted, e, start, Some(fate)));
        }
        // Committed and durable.
        if !retire.is_empty() {
            let mut retired = 0;
            for t in retire {
                if self.dict.retire(t.space, t.id) {
                    retired += 1;
                }
            }
            if retired > 0 {
                self.touched();
            }
            self.dict.release();
        }
        drop(log);
        drop(minted);
        // Committed: its redo records and its statements' update-log
        // records are appended now, after the commit point (review
        // durability#6).
        let logged = self.log_commit();
        self.run_deferred_rebuild();
        logged.map_err(CommitFailure::CommittedWithError)
    }

    /// The commit point of a closing transaction failed with `e`: roll it
    /// back when nothing of it can have reached the store, else keep it
    /// applied and report it as not durable. `start` is the
    /// [`wal_mark`](Self::wal_mark) before the commit; `fate` what the
    /// failed commit point left ([`commit_point_fate`](Self::commit_point_fate);
    /// `None` when the commit failed before it, applying a Stream-mode
    /// store's overlay).
    ///
    /// The logs follow what a reopen keeps (round-3 review): a transaction
    /// whose whole batch the store's write-ahead log holds — the next open
    /// replays it — is appended to the redo and update logs; one whose
    /// outcome is unknown (a part may survive; RocksDB: its journal was
    /// deleted, committing it, but the log sync failed) is left out and
    /// the logs are marked in doubt.
    fn failed_commit(
        &mut self,
        log: Vec<UndoOp>,
        minted: Vec<crate::dict::TermId>,
        e: GStoreError,
        start: WalMark,
        fate: Option<CommitFate>,
    ) -> CommitFailure {
        let fate = fate.unwrap_or_else(|| self.failed_point_fate(start, None));
        if fate == CommitFate::NotApplied {
            return self.undo_failed_commit(log, minted, e, RocksUndo::Rollback);
        }
        // Applied: its frees are not made (nothing references them less
        // than before the failure would justify). Committed records still
        // owed to the logs are not its own: they stay owed (review db#2).
        self.reclaim.clear();
        let e = match fate {
            CommitFate::Replayed => match self.log_commit() {
                Ok(()) => e,
                Err(le) => GStoreError::Database(format!("{e}; in addition, {le}")),
            },
            _ => {
                self.redo_pending.clear();
                self.update_pending.clear();
                self.note_logs_in_doubt(&e);
                e
            }
        };
        self.run_deferred_rebuild();
        CommitFailure::NotDurable(e)
    }

    /// Undo a transaction whose commit failed with `e` (see
    /// [`undo_transaction`](Self::undo_transaction)).
    fn undo_failed_commit(
        &mut self,
        log: Vec<UndoOp>,
        minted: Vec<crate::dict::TermId>,
        e: GStoreError,
        rocks: RocksUndo,
    ) -> CommitFailure {
        match self.undo_transaction(log, minted, true, rocks) {
            Ok(()) => CommitFailure::RolledBack(e),
            // Undone; only deleting the replayed RocksDB journal failed (the
            // next write or open deletes it — replaying it again is a no-op).
            Err((true, cleanup)) => CommitFailure::RolledBack(GStoreError::Database(format!(
                "{e}; (the rolled-back transaction's journal is deleted later: {cleanup})"
            ))),
            Err((false, undo)) => CommitFailure::NotDurable(GStoreError::Database(format!(
                "{e}; rolling the transaction back failed too: {undo} (its journal is replayed \
                 by the next write or open, which undoes it)"
            ))),
        }
    }

    /// The on-disk store's flushes that got as far as naming their
    /// write-ahead log, and those whose name was made durable (both modes;
    /// zeros for the other layouts): taken before a commit, they tell what
    /// a failed commit point left ([`failed_point_fate`](Self::failed_point_fate)).
    fn wal_mark(&self) -> WalMark {
        let (mut wal, mut durable) = self.disk_guard().as_ref().map_or((0, 0), |s| {
            (s.store.wal_batches(), s.store.durable_wal_batches())
        });
        if let Some(dt) = self.store.as_disk() {
            wal += dt.wal_batches();
            durable += dt.durable_wal_batches();
        }
        WalMark { wal, durable }
    }

    /// Whether this database has an on-disk (`kvstore.kv`) store: the one
    /// layout whose failed commit points [`wal_mark`](Self::wal_mark) can
    /// tell apart.
    fn has_paged_store(&self) -> bool {
        self.disk_guard().is_some() || self.store.as_disk().is_some()
    }

    /// Run a commit point (outside a transaction, or a transaction's own)
    /// and say what it left of the commit begun at `start` (see
    /// [`CommitFate`]).
    fn commit_point_fate(
        &self,
        start: WalMark,
    ) -> std::result::Result<(), (GStoreError, CommitFate)> {
        let healthy = self.storage_failure().is_none();
        let pre = self.wal_mark();
        let (res, only_flush) = self.commit_point_report();
        res.map_err(|e| {
            let fate = self.failed_point_fate(start, Some((pre, healthy && only_flush)));
            (e, fate)
        })
    }

    /// What a failed commit left of the commit begun at `start`: nothing
    /// (no write-ahead log got a name since), all of it (the commit point's
    /// own flush failed after its log's name was durable, the store was
    /// healthy before it and nothing else failed — `point`: the mark before
    /// the commit point and whether those hold), or an unknown part.
    fn failed_point_fate(&self, start: WalMark, point: Option<(WalMark, bool)>) -> CommitFate {
        if !self.has_paged_store() {
            return CommitFate::InDoubt;
        }
        let now = self.wal_mark();
        if now.wal == start.wal {
            return CommitFate::NotApplied;
        }
        match point {
            Some((pre, true))
                if now.wal > pre.wal && now.wal - pre.wal == now.durable - pre.durable =>
            {
                CommitFate::Replayed
            }
            _ => CommitFate::InDoubt,
        }
    }

    /// Mark the logs in doubt after a commit point failed with `e` (see
    /// [`logs_in_doubt`](Self::logs_in_doubt)); the first reason is kept.
    fn note_logs_in_doubt(&self, e: &GStoreError) {
        if self.redo_log.is_none() && self.update_log.is_none() {
            return;
        }
        let mut slot = self.logs_in_doubt.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_none() {
            *slot = Some(e.to_string());
        }
    }

    /// Roll back the active transaction, undoing every triple mutation made
    /// since [`begin`](Self::begin) in reverse order. A transaction that
    /// changed nothing (only reads, or writes that changed no triple) leaves
    /// [`mutation_count`](Self::mutation_count), the cached query results
    /// and the CSR as they were.
    pub fn rollback(&mut self) -> Result<()> {
        let Some(log) = self.txn.take() else {
            return Err(GStoreError::Database("no active transaction".into()));
        };
        let minted = self.dict.take_allocations();
        self.dict.track_allocations(false);
        // Stream mode: the default graph's changes live in the overlay, not
        // in the undo log.
        let overlay = self
            .store
            .as_disk()
            .and_then(|dt| dt.transaction())
            .is_some_and(|t| !t.is_empty());
        self.undo_transaction(log, minted, overlay, RocksUndo::Rollback)
            .map_err(|(_, e)| e)
    }

    /// Undo the transaction that just closed: its undo `log` in reverse
    /// order, the terms it `minted` freed once its inserts are gone. `rocks`
    /// names the journal step that undoes a RocksDB transaction's writes.
    /// `changed`: whether it changed anything the log does not show (a
    /// Stream-mode overlay, or a commit that got as far as applying it);
    /// without that and with an empty log, nothing counts as a mutation. An
    /// error says whether the writes were undone all the same (only the
    /// deletion of a replayed RocksDB journal failed).
    fn undo_transaction(
        &mut self,
        log: Vec<UndoOp>,
        minted: Vec<crate::dict::TermId>,
        changed: bool,
        rocks: RocksUndo,
    ) -> std::result::Result<(), (bool, GStoreError)> {
        let changed = changed || !log.is_empty();
        // Its deletes are undone (nothing to free); the terms it minted are
        // freed once its inserts are gone.
        self.reclaim.clear();
        // The transaction's mutations are being undone, so its buffered redo
        // and update-log records must never reach the logs, nor its queued
        // write-through the on-disk store (which never saw them). Records
        // of earlier commits whose append failed are owed by the log
        // writers, not buffered here: they are kept (review db#2).
        self.redo_pending.clear();
        self.update_pending.clear();
        self.disk_discard_pending();
        // Stream mode: the overlay (default graph and queued quads) is just
        // dropped; only the in-memory named graphs are undone below.
        if let Some(dt) = self.store.as_disk_mut() {
            dt.rollback();
        }
        // A RocksDB store is rolled back by replaying its undo journal (which
        // also covers the named-graph quads in RocksDB); only the in-memory
        // named-graph map is undone here.
        let journaled = match rocks {
            RocksUndo::Rollback => self.rocks_journaling(),
            RocksUndo::FailedCommit => true,
        };
        // Default-graph triples the rollback puts back: their edges are folded
        // into the tree once the store holds them again (below), O(height)
        // each.
        let mut readded: Vec<IdTriple> = Vec::new();
        // An in-memory store undoes each run of consecutive default-graph
        // re-adds (or removals) as one batch: same result as one by one.
        let batched = !journaled && self.store.as_memory().is_some();
        let mut ops = log.into_iter().rev().peekable();
        while let Some(op) = ops.next() {
            match op {
                UndoOp::Add(None, id) if batched => {
                    let mut run = vec![id];
                    while let Some(&UndoOp::Add(None, next)) = ops.peek() {
                        run.push(next);
                        ops.next();
                    }
                    let added = self.store.insert_many(&run);
                    readded.extend(
                        run.into_iter()
                            .zip(added)
                            .filter_map(|(t, a)| a.then_some(t)),
                    );
                }
                UndoOp::Del(None, id) if batched => {
                    let mut run = vec![id];
                    while let Some(&UndoOp::Del(None, next)) = ops.peek() {
                        run.push(next);
                        ops.next();
                    }
                    let gone = self
                        .store
                        .remove_many(&run)
                        .into_iter()
                        .filter(|&g| g)
                        .count();
                    if self.index_valid {
                        self.vs_shrink_debt += gone as u64;
                    }
                }
                UndoOp::Add(None, id) => {
                    if journaled || self.store.insert(id) {
                        readded.push(id);
                    }
                }
                UndoOp::Del(None, id) => {
                    // Removing the transaction's insert shrinks signatures:
                    // the tree stays a sound superset (see `remove_default_id`).
                    if (journaled || self.store.remove(id)) && self.index_valid {
                        self.vs_shrink_debt += 1;
                    }
                }
                UndoOp::Add(Some(gid), id) => {
                    if self.named.insert(gid, id) && !journaled {
                        self.persist_named_quad(true, gid, id);
                    }
                }
                UndoOp::Del(Some(gid), id) => {
                    if self.named.remove(gid, id) && !journaled {
                        self.persist_named_quad(false, gid, id);
                    }
                }
            }
        }
        let replayed = match rocks {
            RocksUndo::Rollback => self.rocks_rollback_journal(),
            RocksUndo::FailedCommit => self.rocks_rollback_uncleared_journal(),
        };
        if replayed.is_err() {
            // The store's state is not known: never filter through the tree.
            self.index_valid = false;
        } else if self.index_valid && !readded.is_empty() {
            if self.vstree.is_disk_backed() {
                // A read-only tree cannot take the grown signatures.
                self.index_valid = false;
            } else {
                for id in readded {
                    self.vstree_index_insert(id);
                }
            }
        }
        // Review db#7: a transaction that changed nothing invalidates no
        // cached answer and no CSR (the ids it minted, if any, are freed
        // below, which counts on its own).
        if changed || replayed.is_err() {
            self.touched();
        }
        if replayed.is_ok() && self.id_reclaim == IdReclaim::Immediate {
            for t in minted {
                self.reclaim.note(t);
            }
        }
        // RocksDB: the replayed journal is still on disk; it is deleted in
        // one batch with the minted and replayed terms nothing references
        // any more, so a crash before that batch replays it again at the
        // next open (which frees them then) — never an orphan term.
        let undone = match replayed {
            Ok(()) if journaled => {
                let dead = self.committed_dead();
                let cleared = self.rocks_commit_journal_with(&dead);
                if cleared.is_ok() {
                    self.free_dead(dead, true);
                }
                cleared.map_err(|e| (true, e))
            }
            Ok(()) => {
                if self.id_reclaim == IdReclaim::Immediate {
                    self.flush_reclaim();
                }
                Ok(())
            }
            Err(e) => Err((false, e)),
        };
        self.run_deferred_rebuild();
        undone
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
        Ok(self.insert_quad_id(t, graph)?.is_some())
    }

    /// [`insert_quad`](Self::insert_quad), returning the graph (`None` =
    /// default) and ids of a newly added statement.
    fn insert_quad_id(
        &mut self,
        t: &Triple,
        graph: Option<&str>,
    ) -> Result<Option<(Option<u32>, IdTriple)>> {
        let Some(g) = graph else {
            return Ok(self.try_insert_triple_id(t)?.map(|id| (None, id)));
        };
        let Some((Some(gid), id)) = self.intern_for_insert(t, Some(g))? else {
            return Ok(None);
        };
        Ok(self.insert_named_id(gid, id).then_some((Some(gid), id)))
    }

    /// Add statement `id` to named graph `gid`, whose terms are interned (and
    /// persisted) already; `true` if it was newly added (the mirror of
    /// [`remove_named_id`](Self::remove_named_id)).
    fn insert_named_id(&mut self, gid: u32, id: IdTriple) -> bool {
        let present = self
            .named
            .get(&gid)
            .is_some_and(|s| s.exists(id.sub, id.pred, id.obj));
        // RocksDB first: if the quad cannot be persisted it is not applied at
        // all (so a retry writes it again), and the error is reported at the
        // next commit point.
        if present || !self.persist_named_quad(true, gid, id) {
            return false;
        }
        self.named.insert(gid, id);
        self.touched();
        if let Some(log) = self.txn.as_mut() {
            log.push(UndoOp::Del(Some(gid), id));
        }
        self.record_redo(b'I', Some(gid), id);
        self.persist_mutation(true, Some(gid), id);
        true
    }

    /// Remove a triple from a named graph (`None` ⇒ default graph). Returns
    /// `true` if it existed.
    fn delete_quad(&mut self, t: &Triple, graph: Option<&str>) -> bool {
        let Some(g) = graph else {
            return self.remove_triple_quiet(t);
        };
        let (Some(gid), Some(sub), Some(pred), Some(obj)) = (
            self.dict.entity_id(&Term::iri(g).dict_key()),
            self.dict.entity_id(&t.subject.dict_key()),
            self.dict.predicate_id(&t.predicate.dict_key()),
            self.dict.term_id(&t.object),
        ) else {
            return false;
        };
        self.remove_named_id(gid, IdTriple::new(sub, pred, obj))
    }

    /// Remove statement `id` from named graph `gid`; `true` if it existed.
    fn remove_named_id(&mut self, gid: u32, id: IdTriple) -> bool {
        let present = self
            .named
            .get(&gid)
            .is_some_and(|s| s.exists(id.sub, id.pred, id.obj));
        // RocksDB first (see `insert_quad`).
        if !present || !self.persist_named_quad(false, gid, id) {
            return false;
        }
        // An emptied graph is dropped: this store keeps no empty named
        // graphs (`GRAPH ?g` never finds one, and none survives a reopen).
        self.named.remove(gid, id);
        self.touched();
        if let Some(log) = self.txn.as_mut() {
            log.push(UndoOp::Add(Some(gid), id));
        }
        self.record_redo(b'D', Some(gid), id);
        self.persist_mutation(false, Some(gid), id);
        self.note_removed(Some(gid), id);
        true
    }

    /// Apply `INSERT DATA`. Returns the number of triples newly added. A triple
    /// that cannot be encoded (dictionary id space exhausted) is skipped; see
    /// [`try_insert_data`](Self::try_insert_data) to surface that as an error.
    pub fn insert_data(&mut self, triples: &[GroundTriple]) -> usize {
        // One redo-log commit for the whole call (review db#8).
        self.redo_batch(|this| {
            triples
                .iter()
                .filter(|g| {
                    this.insert_quad(&Self::ground_to_triple(g), g.graph.as_deref())
                        .unwrap_or(false)
                })
                .count()
        })
    }

    /// Apply `INSERT DATA`, stopping with an error if a dictionary id space is
    /// exhausted (triples before the failing one stay inserted, as with any
    /// mid-update failure; wrap in a transaction to make it atomic).
    pub fn try_insert_data(&mut self, triples: &[GroundTriple]) -> Result<usize> {
        let mut added = Vec::new();
        let quads = triples
            .iter()
            .map(|g| (Self::ground_to_triple(g), g.graph.as_deref()));
        // One redo-log commit for the whole call (review db#8).
        self.redo_batch(|this| this.insert_statements(quads, &mut added))?;
        Ok(added.len())
    }

    /// Apply `DELETE DATA`. Returns the number of triples actually removed.
    pub fn delete_data(&mut self, triples: &[GroundTriple]) -> usize {
        let quads = triples
            .iter()
            .map(|g| (Self::ground_to_triple(g), g.graph.as_deref()));
        // One redo-log commit for the whole call (review db#8).
        let (n, _) = self.redo_batch(|this| this.remove_statements(quads, None));
        self.flush_reclaim();
        n
    }

    /// Materialize the RDFS closure over the current data (gStore `src/Reason`):
    /// subclass/subproperty transitivity, type propagation, and domain/range
    /// typing. Returns the number of inferred triples added. An in-memory
    /// VS-tree takes them as it takes inserts; a disk-backed one is rebuilt
    /// in memory at the end of the run (Stream mode: by
    /// [`rebuild_index`](Self::rebuild_index) or at the next open).
    ///
    /// A read error of the store (Stream mode, RocksDB) ends the run early
    /// and is only logged, so the closure may be incomplete:
    /// [`try_materialize_rdfs`](Self::try_materialize_rdfs) reports it.
    pub fn materialize_rdfs(&mut self) -> usize {
        let (n, error) = self.materialize_rdfs_scoped();
        if let Some(e) = error {
            crate::gs_warn!("materializing the RDFS closure of '{}': {e}", self.name);
        }
        n
    }

    /// [`materialize_rdfs`](Self::materialize_rdfs), reporting a read error
    /// of the store (contract C2; review design#7): the run reads inside a
    /// [`ReadScope`], and an error recorded there fails it. The triples
    /// inferred before the failure stay inserted — each is entailed by the
    /// data, so the store stays sound, only its closure incomplete — and a
    /// later run, once the store reads cleanly, completes it.
    pub fn try_materialize_rdfs(&mut self) -> Result<usize> {
        match self.materialize_rdfs_scoped() {
            (n, None) => Ok(n),
            (_, Some(e)) => Err(e),
        }
    }

    /// The RDFS run of [`materialize_rdfs`](Self::materialize_rdfs): the
    /// number of triples inferred, and the read error that ended it early.
    fn materialize_rdfs_scoped(&mut self) -> (usize, Option<GStoreError>) {
        self.infer_scoped("materializing the RDFS closure", |dict, store, insert| {
            crate::reason::materialize_with(dict, store, insert)
        })
    }

    /// Run reasoner `run` ([`infer`](Self::infer)) and record what it
    /// inferred ([`record_inferred`](Self::record_inferred)) inside a
    /// [`ReadScope`], then run a VS-tree rebuild that is owed. Returns how
    /// many triples were inferred, and the read error the scope recorded
    /// (`what` names the run in it), which ended the run early: a backend
    /// answers a failed read as "nothing there".
    fn infer_scoped<R>(&mut self, what: &str, run: R) -> (usize, Option<GStoreError>)
    where
        R: FnOnce(
            &mut Dictionary,
            &mut Backend,
            &mut dyn FnMut(&Dictionary, &mut Backend, IdTriple) -> bool,
        ) -> Vec<IdTriple>,
    {
        let scope = ReadScope::enter();
        let added = self.infer(run);
        let n = self.record_inferred(added);
        let error = scope.take_error();
        drop(scope);
        if error.is_some() && n > 0 {
            // A fold into the VS-tree that read nothing would leave a
            // signature short of its edges: rebuild before filtering again.
            self.mark_vstree_stale();
        }
        self.run_deferred_rebuild();
        (n, error.map(|e| read_failure(what, e)))
    }

    /// Book-keeping after reasoning inserted `added` into the default graph:
    /// the VS-tree (an in-memory tree takes them as it takes inserts, a
    /// disk-backed one is owed a rebuild — review db#4), the caches, the
    /// undo log for rollback, the redo log and the write-through (on-disk).
    /// Returns how many there were.
    fn record_inferred(&mut self, added: Vec<IdTriple>) -> usize {
        let n = added.len();
        if n > 0 {
            if self.index_valid && !self.vstree.is_disk_backed() {
                // Every inferred triple is in the store already: each fold
                // re-signs from it or ORs the edge in, a sound superset.
                for &t in &added {
                    self.vstree_index_insert(t);
                }
            } else {
                self.mark_vstree_stale();
            }
            self.touched();
            // Record inferred triples so reasoning inside a transaction rolls
            // back (Stream mode: they are in the store's overlay).
            let disk = self.store.as_disk().is_some();
            if let Some(log) = self.txn.as_mut().filter(|_| !disk) {
                for t in &added {
                    log.push(UndoOp::Del(None, *t));
                }
            }
            // Inferred triples are mutations like any other: their redo
            // records (review db#3), appended together at the end of the
            // run (inside a transaction: at its commit).
            let held = std::mem::replace(&mut self.redo_hold, true);
            for &t in &added {
                self.record_redo(b'I', None, t);
            }
            self.redo_hold = held;
            if !held && self.txn.is_none() {
                // Like a direct-API write (no commit point of its own):
                // `fdatasync`ed by the next one; a failed append is owed.
                let _ = self.flush_redo(false);
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
    /// same request before it stay applied, as after any failure. A `LOAD`
    /// checks the flag between import batches and undoes what it inserted.
    /// Mirrors gStore's task cancellation.
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
        let stmt = Statement::parse(sparql)?;
        self.execute_as(stmt, cancel, pfn)
    }

    /// Run a parsed [`Statement`]: a read through [`read_as`](Self::read_as),
    /// an UPDATE through the write path (the update log records its text).
    /// What [`query_as`](Self::query_as) does after parsing — a server that
    /// parsed the request to route it (shared reads, exclusive writes) runs
    /// it with this without parsing it again.
    pub fn execute_as(
        &mut self,
        stmt: Statement,
        cancel: Option<Arc<AtomicBool>>,
        pfn: Option<&crate::pfn::PfnCaller>,
    ) -> Result<QueryResult> {
        if !stmt.is_update() {
            return self.read_as(&stmt, cancel, pfn);
        }
        let (text, query) = stmt.into_parts();
        match query {
            // An UPDATE's WHERE clause calls the same caller's functions.
            Query::Update(ops) => self.run_update(&text, ops, cancel, pfn),
            _ => Err(GStoreError::Query("not an update".into())),
        }
    }

    /// Parse and run a **read** request — SELECT, ASK, CONSTRUCT or DESCRIBE —
    /// on a shared `&Database`, so several threads may query one database at
    /// once (`Database` is `Send + Sync`). An UPDATE is refused with an error
    /// before anything happens; run it through [`query`](Self::query) /
    /// [`execute_as`](Self::execute_as). Otherwise exactly
    /// [`query_as`](Self::query_as): the same cache, VS-tree, named graphs,
    /// functions, PFN caller and limits, `SERVICE` policy and cancellation.
    pub fn query_read(
        &self,
        sparql: &str,
        cancel: Option<Arc<AtomicBool>>,
        pfn: Option<&crate::pfn::PfnCaller>,
    ) -> Result<QueryResult> {
        let stmt = Statement::parse(sparql)?;
        self.read_as(&stmt, cancel, pfn)
    }

    /// This database's evaluation settings, as every evaluator it builds
    /// applies them ([`Evaluator::with_settings`]): the `SERVICE` policy of
    /// its [`LoadPolicy`], the integer-division mode, the Top-K options, the
    /// graph-function output bound, the PFN caller and limits and the
    /// user-defined functions. A read snapshot (an HTTP transaction's, a
    /// [`ConcurrentDb`](crate::ConcurrentDb) version) stores this value and
    /// applies it to its own reads, so they follow the same settings
    /// (review design#9).
    pub fn eval_settings(&self) -> crate::query::engine::EvalSettings {
        crate::query::engine::EvalSettings {
            service_policy: self.load_policy.service_policy(),
            integer_division: self.integer_division,
            topk: self.topk,
            gfun_max_output_bytes: self.gfun_max_output_bytes,
            path_max_duplicates: crate::query::engine::PATH_MAX_DUPLICATES,
            pfn_limits: self.pfn_limits,
            pfn: self.pfn.clone(),
            functions: self.functions.clone(),
            vs_policy: None,
        }
    }

    /// Run the parsed read request `stmt` on a shared `&Database` (see
    /// [`query_read`](Self::query_read)); an UPDATE is refused with no side
    /// effect. The evaluation runs inside a [`ReadScope`]: a read error a
    /// backend reports fails the query (and its result is not cached).
    pub fn read_as(
        &self,
        stmt: &Statement,
        cancel: Option<Arc<AtomicBool>>,
        pfn: Option<&crate::pfn::PfnCaller>,
    ) -> Result<QueryResult> {
        if stmt.is_update() {
            return Err(GStoreError::Query(
                "an UPDATE cannot run on the shared read path; run it through query / \
                 execute_as"
                    .into(),
            ));
        }
        self.check_store_readable()?;
        let (sparql, q) = (stmt.text(), stmt.query());
        // Serve from the query cache when the same request was already
        // answered against the current (unchanged) store. Only
        // deterministic queries are cached (no NOW()/RAND()/UUID()/
        // STRUUID()/BNODE(), SERVICE or user functions).
        let cacheable = query_cache::is_cacheable(q);
        if cacheable {
            // The guard is released at the end of this statement; the hit is
            // copied out of its `Arc` after that.
            let hit = self.query_cache().get(sparql, self.mutations);
            if let Some(cached) = hit {
                return Ok(Arc::unwrap_or_clone(cached));
            }
        }
        // The advanced graph functions run on this database's cached
        // CSR (see `csr`), built only when a query needs it; substring
        // filters on its cached dictionary index (see `string_index`).
        let flag = cancel.clone();
        let csr = || self.csr_cancellable(flag.as_deref());
        let string_index = || self.string_index_cancellable(flag.as_deref());
        // Use the VS-tree as a candidate filter only while it is
        // consistent with the store; otherwise evaluate without it.
        let mut eval = if self.vstree_usable() {
            Evaluator::with_vstree(&self.dict, &self.store, &self.vstree)
        } else {
            Evaluator::new(&self.dict, &self.store)
        };
        if !self.named.is_empty() {
            eval = eval.with_named(&self.named);
        }
        // This database's settings — `SERVICE` only where its load policy
        // allows remote sources, user-defined functions, gStore's PFN
        // functions as the request's caller (else the installed one)
        // under this database's limits (never cached, see
        // `query_cache::is_cacheable`), … — in one call (review design#9).
        eval = eval.with_settings(
            self.eval_settings(),
            EvalProviders {
                csr: Some(&csr),
                string_index: Some(&string_index),
            },
            pfn,
        );
        if let Some(c) = cancel {
            eval = eval.with_cancel(c);
        }
        // A read error a backend reports fails the query (C2).
        let scope = ReadScope::enter();
        let evaluated = eval.evaluate(q);
        if let Some(e) = scope.take_error() {
            return Err(read_failure("evaluating the query", e));
        }
        drop(scope);
        let result = evaluated?;
        if cacheable && self.query_cache().budget() > 0 {
            // Size the entry before taking the lock; the cache shares the
            // result, so it is copied (once) only if it was cached.
            let bytes = query_cache::entry_cost(sparql, &result);
            if !self.query_cache().admits(bytes) {
                return Ok(result);
            }
            let shared = Arc::new(result);
            self.query_cache().put_sized(
                sparql.to_string(),
                Arc::clone(&shared),
                self.mutations,
                bytes,
            );
            return Ok(Arc::unwrap_or_clone(shared));
        }
        Ok(result)
    }

    /// Apply the parsed operations `ops` of UPDATE request `sparql` (the
    /// text the update log records): the write path of
    /// [`query`](Self::query), shared with the reasoning rules
    /// ([`reason_execute`](Self::reason_execute) /
    /// [`reason_disable`](Self::reason_disable)). A `PFN.name({…})` call in a
    /// WHERE clause reaches `pfn`'s functions, else the
    /// [installed caller](Self::set_pfn_caller)'s.
    fn run_update(
        &mut self,
        sparql: &str,
        ops: Vec<UpdateOp>,
        cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        pfn: Option<&crate::pfn::PfnCaller>,
    ) -> Result<QueryResult> {
        // A broken on-disk store refuses every statement until reopened.
        self.check_store_readable()?;
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
        // RocksDB, outside a transaction: the request is journaled, so a
        // crash part-way undoes all of it (crash-atomic as a whole, whatever
        // its size — the disk layout is so only while its dirty pages fit
        // the page cache, D12); the journal is deleted at its commit point
        // (`apply_update_ops`), or when it fails or panics.
        let journaled = self.rocks_begin_statement()?;
        // A request that panics part-way keeps what it applied, as one that
        // fails does (RocksDB: its journal is deleted on the way out): its
        // redo records are logged then, and the redo hold that
        // `apply_update_ops` set ends with it — a hold left behind would
        // keep later direct-API writes' records back, for a rollback to
        // drop (review durability#6, db#2).
        let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.rocks_statement_body(journaled, |db| {
                db.apply_update_ops(sparql, ops, cancel, pfn, journaled)
            })
        }));
        done.unwrap_or_else(|panic| {
            if self.redo_hold {
                // No commit point ran: what it applied stays, and is logged
                // as before unless the store broke (then nothing of it is
                // known to survive a reopen).
                let fate = if self.storage_failure().is_some() {
                    CommitFate::InDoubt
                } else {
                    CommitFate::Durable
                };
                let _ = self.end_statement_logs(None, fate);
            }
            std::panic::resume_unwind(panic)
        })
    }

    /// The body of [`run_update`](Self::run_update): apply the request's
    /// operations in order, then its update log line and commit point
    /// (`journaled`: the end of its RocksDB statement first).
    fn apply_update_ops(
        &mut self,
        sparql: &str,
        ops: Vec<UpdateOp>,
        cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        pfn: Option<&crate::pfn::PfnCaller>,
        journaled: bool,
    ) -> Result<QueryResult> {
        let before = self.mutations;
        let start = self.wal_mark();
        let mut changed = 0;
        // The request's redo records are appended together at its end,
        // after its commit point (review durability#6, db#8).
        self.redo_hold = true;
        for op in ops {
            // Each operation is applied inside a read scope (C2): an
            // out-of-core dictionary's read error while a DELETE, CLEAR or
            // DROP looks up its terms would otherwise read as "no such
            // term" and the statement succeed with nothing removed. A store
            // that broke during the operation (a write error inside a
            // transaction, reported as "not applied") fails it as well.
            let scope = ReadScope::enter();
            let applied = self.exec_update_op(op, cancel.as_ref(), pfn);
            let applied = match scope.take_error() {
                Some(e) => Err(read_failure("applying the update", e)),
                None => applied.and_then(|n| self.check_store_readable().map(|()| n)),
            };
            drop(scope);
            // The ids the operation's deletes left unreferenced are freed
            // after its redo records and write-through (deferred to the
            // commit inside a transaction) — also when it failed part-way.
            self.flush_reclaim();
            match applied {
                Ok(n) => changed += n,
                Err(e) => {
                    let (e, fate) = self.failed_update(e, before, start, journaled);
                    // The part applied before the failure stays (committed,
                    // outside a transaction): its redo records are logged.
                    return Err(match self.end_statement_logs(None, fate) {
                        Ok(()) => e,
                        Err(le) => GStoreError::Database(format!("{e}; in addition, {le}")),
                    });
                }
            }
        }
        // Outside a transaction each UPDATE request is a commit point:
        // its write-through is made durable before it is acknowledged
        // (RocksDB: its journal is deleted first, with the ids it freed).
        let point = if journaled {
            // RocksDB: a journal that could not be deleted is undone by the
            // next open (unless a later write deletes it), a failed log
            // sync may or may not keep the request: either way its
            // outcome is unknown.
            let ended = self.end_update_statement(before);
            let point = self.commit_point();
            ended.and(point).map_err(|e| (e, CommitFate::InDoubt))
        } else if self.txn.is_none() {
            self.commit_point_fate(start)
        } else {
            Ok(())
        };
        let (point, fate) = match point {
            Ok(()) => (Ok(()), CommitFate::Durable),
            Err((e, fate)) => (Err(e), fate),
        };
        // Then its redo records and its update-log record (inside a
        // transaction: at the commit), never before the commit point
        // (review durability#6), and only when the commit survives a
        // reopen (round-3 review).
        let logged = self.end_statement_logs(Some((sparql, changed)), fate);
        point?;
        logged.map_err(|e| {
            GStoreError::Database(format!("the update is applied and committed, but {e}"))
        })?;
        Ok(QueryResult::Update { changed })
    }

    /// End a journaled UPDATE request on RocksDB
    /// ([`rocks_end_statement`](Self::rocks_end_statement)). When it changed
    /// nothing — [`mutation_count`](Self::mutation_count) is still `before`,
    /// e.g. a `LOAD` that failed and undid itself — the ids it frees (the
    /// terms that `LOAD` minted) do not count as a change either: the count
    /// and a CSR built before the request are put back, as
    /// [`load_rdf_source`](Self::load_rdf_source) does for its own undo.
    fn end_update_statement(&mut self, before: u64) -> Result<()> {
        if self.mutations != before {
            return self.rocks_end_statement();
        }
        let csr = self.csr_cache.peek(before);
        let ended = self.rocks_end_statement();
        self.mutations = before;
        if let Some(g) = csr {
            self.csr_cache.restore(before, g);
        }
        ended
    }

    /// An UPDATE failed with `e` after its earlier operations (or part of the
    /// failing one) may already have been applied — they stay applied, as
    /// documented on [`try_insert_data`](Self::try_insert_data). Outside a
    /// transaction the applied part is still made durable (this is the
    /// request's commit point), so the persistent store never lags what
    /// readers see; `before` is the [`mutation_count`](Self::mutation_count)
    /// the update started at. A RocksDB statement's journal (`journaled`) is
    /// deleted first, with the ids the statement freed: from then on a crash
    /// no longer undoes the applied part.
    fn failed_update(
        &mut self,
        e: GStoreError,
        before: u64,
        start: WalMark,
        journaled: bool,
    ) -> (GStoreError, CommitFate) {
        let ended = if journaled {
            self.end_update_statement(before)
        } else {
            Ok(())
        };
        if self.txn.is_some() || (self.mutations == before && !journaled) {
            return (e, CommitFate::Durable);
        }
        let point = if journaled {
            ended
                .and_then(|()| self.commit_point())
                .map_err(|pe| (pe, CommitFate::InDoubt))
        } else {
            self.commit_point_fate(start)
        };
        match point {
            Ok(()) => (e, CommitFate::Durable),
            Err((pe, fate)) => (
                GStoreError::Database(format!(
                    "{e}; in addition, the part of the update applied before the failure could \
                     not be persisted: {pe}"
                )),
                fate,
            ),
        }
    }

    /// Apply one UPDATE operation, returning the number of triples it changed.
    /// A `DELETE`/`INSERT … WHERE` evaluates its WHERE clause under `cancel`,
    /// a `LOAD` reads its document under it.
    fn exec_update_op(
        &mut self,
        op: UpdateOp,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
        pfn: Option<&crate::pfn::PfnCaller>,
    ) -> Result<usize> {
        match op {
            UpdateOp::InsertData(triples) => {
                // The loader's literal rule (review sparqlconf#12).
                import::check_update_literals(triples.iter().map(|g| &g.object))?;
                self.try_insert_data(&triples)
            }
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
                    pfn,
                )?;
                // The loader's literal rule, before anything is applied
                // (review sparqlconf#12).
                import::check_update_literals(ins.iter().map(|(_, t)| &t.object))?;
                // A template triple outside `GRAPH { … }` targets the WITH
                // graph, else the default graph.
                let target = |g: &Option<String>| g.clone().or_else(|| with.clone());
                let (removed, _) =
                    self.remove_statements(dels.iter().map(|(g, t)| (t, target(g))), None);
                let mut added = Vec::new();
                self.insert_statements(ins.iter().map(|(g, t)| (t, target(g))), &mut added)?;
                Ok(removed + added.len())
            }
            UpdateOp::Load {
                source,
                silent,
                into,
            } => match self.load_rdf_source(&source, into.as_deref(), cancel) {
                Ok(n) => Ok(n),
                // `SILENT` hides the failure of the load (which applied
                // nothing), not a cancel of the request.
                Err(e) if silent && !matches!(e, GStoreError::Cancelled) => Ok(0),
                Err(e) => Err(e),
            },
            // The graph-management operations share one existence rule
            // (SPARQL 1.1 Update §3.2; review sparqlconf#16): a named graph
            // exists while it holds a triple — this store keeps no empty
            // named graphs — and the default graph always exists. An
            // operation on a graph that must exist but does not (CLEAR /
            // DROP GRAPH, the source of ADD / MOVE / COPY), or that must not
            // exist but does (CREATE GRAPH), fails, changing nothing, unless
            // SILENT, which makes it a successful no-op.
            UpdateOp::Clear { target, silent } => self.clear_target("CLEAR", target, silent),
            UpdateOp::Drop { target, silent } => self.clear_target("DROP", target, silent),
            // CREATE GRAPH declares an empty graph, and this store keeps
            // none: creating a graph that does not exist changes nothing
            // (data operations create it), one that exists is an error.
            UpdateOp::Create { name, silent } => {
                if silent || !self.named_graph_exists(&name) {
                    Ok(0)
                } else {
                    Err(GStoreError::Query(format!(
                        "CREATE GRAPH failed: the graph <{name}> already exists (it holds \
                         triples); use CREATE SILENT GRAPH to ignore an existing graph"
                    )))
                }
            }
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
    /// * The same graph on both sides is a no-op, whatever the operation
    ///   (the operation has no effect), even for a graph that does not exist.
    /// * The source must exist — the existence rule of every graph-management
    ///   operation (see [`exec_update_op`](Self::exec_update_op)): a named
    ///   source that holds no triples fails the operation, changing nothing,
    ///   unless SILENT, which makes it a successful no-op. The default graph
    ///   always exists.
    /// * The destination is created as needed.
    /// * The triples are copied by id — every graph shares the dictionary —
    ///   in chunks of [`TRANSFER_CHUNK`]: the source is read once, as ids (12
    ///   bytes a triple, in a read scope: a read error fails the operation
    ///   before anything changes), and never turned into terms that are
    ///   interned again one by one (review db#16). Only the first triple into
    ///   a named destination takes the term path, which interns the
    ///   destination graph's IRI (on RocksDB writing it to the store first).
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
        let Some(ids) = self.graph_triples(from.as_deref())? else {
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
                None => self.clear_default()?,
                Some(g) => self.clear_named(g),
            };
        }
        changed += self.insert_graph_ids(to.as_deref(), &ids)?;
        if op == GraphTransfer::Move {
            changed += match from.as_deref() {
                None => self.clear_default()?,
                Some(g) => self.clear_named(g),
            };
        }
        Ok(changed)
    }

    /// `CLEAR` / `DROP` (`keyword`) `[SILENT] target` (SPARQL 1.1 Update
    /// §3.1.5 / §3.2.2; this store keeps no empty graphs, so the two are the
    /// same): the number of triples cleared. `GRAPH <g>` of a graph that does
    /// not exist (holds no triples) fails, changing nothing, unless SILENT —
    /// the existence rule of [`exec_update_op`](Self::exec_update_op);
    /// `DEFAULT`, `NAMED` and `ALL` always succeed.
    fn clear_target(&mut self, keyword: &str, target: GraphTarget, silent: bool) -> Result<usize> {
        match target {
            GraphTarget::Default => self.clear_default(),
            GraphTarget::All => Ok(self.clear_default()? + self.clear_all_named()),
            GraphTarget::AllNamed => Ok(self.clear_all_named()),
            GraphTarget::Named(g) if self.named_graph_exists(&g) => Ok(self.clear_named(&g)),
            GraphTarget::Named(_) if silent => Ok(0),
            GraphTarget::Named(g) => Err(GStoreError::Query(format!(
                "{keyword} GRAPH failed: the graph <{g}> does not exist (it holds no triples); \
                 use {keyword} SILENT GRAPH to ignore a missing graph"
            ))),
        }
    }

    /// Whether named graph `<g>` exists: it holds a triple (this store keeps
    /// no empty named graphs; review sparqlconf#16).
    fn named_graph_exists(&self, g: &str) -> bool {
        self.dict
            .entity_id(&Term::iri(g).dict_key())
            .and_then(|gid| self.named.get(&gid))
            .is_some_and(|s| s.triple_count() > 0)
    }

    /// Every triple of the default graph (`None`) or of a named graph, as
    /// ids; `Ok(None)` for a named graph that holds no triples. Reads in a
    /// read scope: a store read error fails it (C2).
    fn graph_triples(&self, graph: Option<&str>) -> Result<Option<Vec<IdTriple>>> {
        let scope = ReadScope::enter();
        let triples = self.graph_triples_unscoped(graph);
        match scope.take_error() {
            Some(e) => Err(read_failure("reading the source graph", e)),
            None => Ok(triples),
        }
    }

    /// [`graph_triples`](Self::graph_triples) without the read scope.
    fn graph_triples_unscoped(&self, graph: Option<&str>) -> Option<Vec<IdTriple>> {
        match graph {
            None => Some(self.store.iter_all()),
            Some(g) => {
                let gid = self.dict.entity_id(&Term::iri(g).dict_key())?;
                let ids: Vec<IdTriple> = self.named.get(&gid)?.iter_all().collect();
                (!ids.is_empty()).then_some(ids)
            }
        }
    }

    /// Insert `ids` — triples of another graph of this database, so their
    /// terms are interned and persisted already — into the default graph
    /// (`None`) or named graph `<g>` (see
    /// [`transfer_graph`](Self::transfer_graph)): by id, in chunks of
    /// [`TRANSFER_CHUNK`] (an in-memory default graph takes each chunk as one
    /// batch, [`insert_default_batch`](Self::insert_default_batch)). Returns
    /// how many were newly added; on an error the triples before stay
    /// inserted, as with any failing operation.
    fn insert_graph_ids(&mut self, graph: Option<&str>, ids: &[IdTriple]) -> Result<usize> {
        let mut added = 0;
        let mut rest = ids;
        let gid = match graph {
            None => None,
            Some(g) => {
                // The first triple interns the graph's IRI the way every
                // insert does (RocksDB: written before the quad).
                let Some((&first, tail)) = ids.split_first() else {
                    return Ok(0);
                };
                let t = self.id_triple_terms(first)?;
                added += usize::from(self.insert_quad_id(&t, Some(g))?.is_some());
                rest = tail;
                match self.persisted_graph_id(g) {
                    Some(gid) => Some(gid),
                    None => {
                        // The IRI could not be persisted (RocksDB; reported
                        // at the next commit point): every triple takes the
                        // term path, which writes it again first.
                        for &id in rest {
                            let t = self.id_triple_terms(id)?;
                            added += usize::from(self.insert_quad_id(&t, Some(g))?.is_some());
                        }
                        return Ok(added);
                    }
                }
            }
        };
        let batched = self.store.as_memory().is_some();
        for chunk in rest.chunks(TRANSFER_CHUNK) {
            match gid {
                Some(gid) => {
                    for &id in chunk {
                        added += usize::from(self.insert_named_id(gid, id));
                    }
                }
                None if batched => added += self.insert_default_batch(chunk).len(),
                None => {
                    for &id in chunk {
                        added += usize::from(self.insert_default_id(id)?);
                    }
                }
            }
        }
        Ok(added)
    }

    /// The id of named graph `<g>`'s IRI when the dictionary knows it and
    /// (RocksDB) its write did not fail: quads may then reference it.
    fn persisted_graph_id(&self, g: &str) -> Option<u32> {
        let gid = self.dict.entity_id(&Term::iri(g).dict_key())?;
        #[cfg(feature = "rocksdb")]
        if self
            .unpersisted_terms
            .contains(&(crate::backend::rocks::TermSpace::Entity, gid))
        {
            return None;
        }
        Some(gid)
    }

    /// The terms of triple `id` (read in a read scope: a dictionary read
    /// error fails it, C2).
    fn id_triple_terms(&self, id: IdTriple) -> Result<Triple> {
        let scope = ReadScope::enter();
        let term = |s: Option<TermStr<'_>>| s.and_then(|s| Term::from_dict_key(&s));
        let (s, p, o) = (
            term(self.dict.term(id.sub)),
            term(self.dict.pred(id.pred)),
            term(self.dict.term(id.obj)),
        );
        if let Some(e) = scope.take_error() {
            return Err(read_failure("reading the source graph", e));
        }
        match (s, p, o) {
            (Some(s), Some(p), Some(o)) => Ok(Triple::new(s, p, o)),
            _ => Err(GStoreError::Database(format!(
                "a triple of the source graph ({id:?}) references an id missing from the \
                 dictionary"
            ))),
        }
    }

    /// Clear the default graph, returning the count cleared. Stream mode:
    /// every triple is read (in a read scope: a read error fails the clear
    /// before anything is removed) and removed by id — under one drain of the
    /// store's writer gate, or into the open transaction's overlay.
    fn clear_default(&mut self) -> Result<usize> {
        if self.store.as_disk().is_some() {
            return self.clear_default_disk();
        }
        let n = self.store.triple_count() as usize;
        if n > 0 {
            // Snapshot the triples once: needed for undo, and (for a persistent
            // backend) to delete them so the on-disk store is actually cleared.
            let removed = self.store.iter_all();
            let total = removed.len();
            if let Some(log) = self.txn.as_mut() {
                for t in &removed {
                    log.push(UndoOp::Add(None, *t));
                }
            }
            // The triples the store no longer holds (all of them unless a
            // persistent backend refused a delete).
            let gone = if self.store.as_memory().is_some() {
                // In-memory: discard and replace (cheaper than per-triple delete).
                self.store = Backend::Memory(TripleStore::new());
                // An on-disk database must still delete them from its store.
                if self.has_disk_sink() {
                    for &t in &removed {
                        self.persist_mutation(false, None, t);
                    }
                }
                removed
            } else {
                // Persistent backend: delete every triple through the seam so the
                // change is durable (don't swap in a Memory store, which would
                // strand the data on disk). A refused delete (RocksDB: reported
                // at the next commit point) leaves its triple stored.
                let mut gone = Vec::with_capacity(removed.len());
                for &t in &removed {
                    if self.store.remove(t) {
                        gone.push(t);
                    }
                }
                gone
            };
            // Every entry of the tree is a phantom now: rebuilt from what is
            // left (usually nothing) at the end of this clear, or of the
            // open transaction (review db#4).
            self.mark_vstree_stale();
            self.touched();
            if gone.len() == total {
                self.record_redo_clear(RedoClear::Default);
            } else {
                // Part of the graph stays: the log names exactly what went, so
                // replaying it reproduces this state (not an empty graph).
                for &t in &gone {
                    self.record_redo(b'D', None, t);
                }
            }
            for &t in &gone {
                self.note_removed(None, t);
            }
            self.run_deferred_rebuild();
        }
        Ok(n)
    }

    /// [`clear_default`](Self::clear_default) of a Stream-mode database.
    fn clear_default_disk(&mut self) -> Result<usize> {
        let scope = ReadScope::enter();
        let removed = self.store.iter_all();
        if let Some(e) = scope.take_error() {
            return Err(read_failure("reading the default graph to clear it", e));
        }
        drop(scope);
        if removed.is_empty() {
            return Ok(0);
        }
        let Some(dt) = self.store.as_disk_mut() else {
            return Ok(0);
        };
        let result = dt.remove_many(removed.iter().copied());
        // Whatever happened, the tree may name deleted edges: rebuild before
        // filtering through it again (as for the other layouts). The
        // Stream-mode `vstree.kv` is rewritten by `rebuild_index` or at the
        // next open only (see `mark_vstree_stale`).
        self.mark_vstree_stale();
        let n = match result {
            Ok(n) => n as usize,
            Err(e) => {
                // Part of it may be applied (the store is broken now).
                self.touched();
                return Err(e);
            }
        };
        if n > 0 {
            self.touched();
            self.record_redo_clear(RedoClear::Default);
            for &t in &removed {
                self.note_removed(None, t);
            }
        }
        Ok(n)
    }

    /// Clear one named graph, returning the count cleared.
    fn clear_named(&mut self, graph: &str) -> usize {
        let Some(gid) = self.dict.entity_id(&Term::iri(graph).dict_key()) else {
            return 0;
        };
        let n = self
            .named
            .get(&gid)
            .map_or(0, |s| s.triple_count() as usize);
        if n > 0 {
            if let Some(log) = self.txn.as_mut() {
                if let Some(s) = self.named.get(&gid) {
                    for t in s.iter_all() {
                        log.push(UndoOp::Add(Some(gid), t));
                    }
                }
            }
            let complete = match self.named.remove_graph(gid) {
                Some(removed) => self.persist_cleared_graph(gid, removed),
                None => true,
            };
            self.touched();
            if complete {
                self.record_redo_clear(RedoClear::Named(gid));
            }
        }
        n
    }

    /// Clear every named graph, returning the total count cleared.
    fn clear_all_named(&mut self) -> usize {
        let total: usize = self.named.values().map(|s| s.triple_count() as usize).sum();
        if total > 0 {
            if let Some(log) = self.txn.as_mut() {
                for (&gid, s) in self.named.iter() {
                    for t in s.iter_all() {
                        log.push(UndoOp::Add(Some(gid), t));
                    }
                }
            }
            // Each graph's clear is logged by itself when RocksDB refused a
            // quad delete (the graph keeps that quad): then the log may not
            // claim that every named graph was emptied.
            let mut cleared = Vec::new();
            let mut complete = true;
            for (gid, removed) in self.named.take_all() {
                if self.persist_cleared_graph(gid, removed) {
                    cleared.push(gid);
                } else {
                    complete = false;
                }
            }
            self.touched();
            if complete {
                self.record_redo_clear(RedoClear::AllNamed);
            } else {
                for gid in cleared {
                    self.record_redo_clear(RedoClear::Named(gid));
                }
            }
        }
        total
    }

    /// Persist the removal of named graph `gid` (already taken out of
    /// [`named`](Self::named)) whose quads were `removed`. A quad RocksDB could
    /// not delete is put back into the in-memory graph, so memory never claims
    /// a deletion the store did not make (the error is reported at the next
    /// commit point). The terms of the quads removed are reclamation
    /// candidates. `true` when every quad went; otherwise each quad that
    /// did is logged by itself (redo `D`), and the caller logs no clear of
    /// the graph.
    fn persist_cleared_graph(&mut self, gid: u32, removed: TripleStore) -> bool {
        let mut kept = Vec::new();
        let mut gone = Vec::new();
        for t in removed.iter_all() {
            self.persist_mutation(false, Some(gid), t);
            if self.persist_named_quad(false, gid, t) {
                self.note_removed(Some(gid), t);
                gone.push(t);
            } else {
                kept.push(t);
            }
        }
        if kept.is_empty() {
            return true;
        }
        self.named.bulk_load(gid, kept);
        for t in gone {
            self.record_redo(b'D', Some(gid), t);
        }
        false
    }

    /// Record a `CLEAR` (also `DROP`, and the clears of `MOVE` / `COPY`) in
    /// the redo log, so replaying the log reproduces it: one record per
    /// cleared target, not one per triple.
    fn record_redo_clear(&mut self, target: RedoClear) {
        if self.redo_log.is_none() {
            return;
        }
        let (op, graph) = match target {
            RedoClear::Default => (b'C', None),
            RedoClear::Named(gid) => match self.dict.term(gid) {
                Some(g) => (b'C', Some(strip_angle(&g).to_owned())),
                None => return,
            },
            RedoClear::AllNamed => (b'N', None),
        };
        self.redo_pending.push(RedoRec {
            op,
            graph,
            triple: CLEAR_BODY.to_owned(),
        });
        if self.txn.is_none() && !self.redo_hold {
            let _ = self.flush_redo(false);
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
    /// a read query does, and inside a [`ReadScope`]: a read error a backend
    /// reports fails the operation before anything is applied.
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
        pfn: Option<&crate::pfn::PfnCaller>,
    ) -> Result<(Vec<(Option<String>, Triple)>, Vec<(Option<String>, Triple)>)> {
        let scope = ReadScope::enter();
        let changes = self.eval_modify_changes(
            delete,
            insert,
            pattern,
            with,
            using,
            using_named,
            cancel,
            pfn,
        );
        match scope.take_error() {
            Some(e) => Err(read_failure("evaluating the update's WHERE clause", e)),
            None => changes,
        }
    }

    /// [`modify_changes`](Self::modify_changes) without the read scope.
    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    fn eval_modify_changes(
        &self,
        delete: &[QuadPattern],
        insert: &[QuadPattern],
        pattern: &GraphPattern,
        with: Option<&str>,
        using: &[String],
        using_named: &[String],
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
        pfn: Option<&crate::pfn::PfnCaller>,
    ) -> Result<(Vec<(Option<String>, Triple)>, Vec<(Option<String>, Triple)>)> {
        let graph = |iri: &str| {
            self.dict
                .entity_id(&Term::iri(iri).dict_key())
                .and_then(|gid| self.named.get(&gid).map(|s| (gid, s)))
        };
        fn prepare<'e, S: TripleSource>(
            db: &Database,
            eval: Evaluator<'e, S>,
            providers: EvalProviders<'e>,
            cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
            pfn: Option<&crate::pfn::PfnCaller>,
        ) -> Evaluator<'e, S> {
            // The WHERE clause runs with this database's settings, as reads
            // do (review design#9): IRI function calls reach the
            // user-defined functions; `SERVICE` follows the load policy
            // (refused on a server's restricted policy); PFN calls reach
            // the request's caller (a server passes the authenticated
            // user), else the installed one, under this database's limits;
            // and the request's cancel flag stops it (a graph function in a
            // sub-SELECT included).
            let eval = eval.with_settings(db.eval_settings(), providers, pfn);
            match cancel {
                Some(c) => eval.with_cancel(std::sync::Arc::clone(c)),
                None => eval,
            }
        }
        // The substring index covers the dictionary, which every graph
        // shares; the CSR covers the default graph only.
        let string_index = || self.string_index_cancellable(cancel.map(|c| &**c));
        let dictionary_only = EvalProviders {
            csr: None,
            string_index: Some(&string_index),
        };
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
            let eval = prepare(
                self,
                Evaluator::new(&self.dict, &merged),
                dictionary_only,
                cancel,
                pfn,
            )
            .with_named(&named);
            return eval.eval_update_modify(delete, insert, pattern);
        }
        if let Some(w) = with {
            let empty = TripleStore::new();
            let store = graph(w).map_or(&empty, |(_, s)| s);
            let eval = prepare(
                self,
                Evaluator::new(&self.dict, store),
                dictionary_only,
                cancel,
                pfn,
            )
            .with_named(&self.named);
            return eval.eval_update_modify(delete, insert, pattern);
        }
        let csr = || self.csr_cancellable(cancel.map(|c| &**c));
        let eval = prepare(
            self,
            Evaluator::new(&self.dict, &self.store),
            EvalProviders {
                csr: Some(&csr),
                ..dictionary_only
            },
            cancel,
            pfn,
        )
        .with_named(&self.named);
        eval.eval_update_modify(delete, insert, pattern)
    }

    /// `LOAD <iri> [INTO GRAPH <g>]`: read an RDF document into the store,
    /// subject to the [`LoadPolicy`]. Only local sources are read — a `file:`
    /// IRI or a bare path; remote (`http(s)://`, …) sources are never
    /// retrieved by SPARQL (no network) and error. The document goes through
    /// the bulk loaders' import pipeline: its format follows the extension
    /// (see [`import::RdfFormat`]), it streams in, and its blank nodes get a
    /// scope of their own (two `LOAD`s of one file never share a blank node).
    ///
    /// * Without `into`, the document is a dataset: its default-graph triples
    ///   go to the default graph, and the statements of an N-Quads / TriG
    ///   named graph to that graph.
    /// * With `into` (SPARQL 1.1 Update §3.1.4: the operation inserts the
    ///   document's triples into the graph it names, and changes no other):
    ///   the document's default graph goes to named graph `into`, and the
    ///   statements of its own named graphs are not loaded (counted, and
    ///   reported in the diagnostic log) — as Jena reads a quad document
    ///   into one graph. `LOAD <src>` without `INTO` loads them.
    ///
    /// Strict and atomic: a statement that does not parse, a cancel (checked
    /// between batches) or any other failure fails the `LOAD` and undoes what
    /// it had inserted, so a failed `LOAD` — `SILENT` or not — changes no
    /// graph. The undo list holds 16 bytes per inserted statement. Once the
    /// undo is complete the data is what it was, so the `LOAD` is no change
    /// at all: [`mutation_count`](Self::mutation_count) is put back (servers
    /// neither report a partial update nor save) and so is a CSR built for
    /// the data before it. Returns the number of statements inserted.
    fn load_rdf_source(
        &mut self,
        source: &str,
        into: Option<&str>,
        cancel: Option<&std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<usize> {
        let path = match self.load_policy.resolve(source)? {
            LoadSource::Local(path) => path,
            LoadSource::Remote(_) => {
                return Err(GStoreError::Query(format!(
                    "LOAD of remote source '{source}' is not supported (no network); \
                     use a local file path or file:// IRI"
                )))
            }
        };
        let mutations = self.mutations;
        let csr = self.csr_cache.peek(mutations);
        let mut applied: Vec<(Option<u32>, IdTriple)> = Vec::new();
        let mut skipped = 0u64;
        let result = import::import_sources(
            &[import::Source::File(path)],
            "LOAD",
            &import::ImportOptions::strict(),
            &mut (),
            &mut |batch: &mut Vec<Quad>| {
                if cancel.is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed)) {
                    return Err(GStoreError::Cancelled);
                }
                let quads = batch.drain(..).filter_map(|q| {
                    let graph = match (into, graph_iri(&q.graph)) {
                        (None, g) => g,
                        (Some(target), None) => Some(target.to_string()),
                        (Some(_), Some(_)) => {
                            skipped += 1;
                            return None;
                        }
                    };
                    Some((q.into_triple(), graph))
                });
                self.insert_statements(quads, &mut applied)
            },
        );
        if let Err(e) = result {
            let inserted = !applied.is_empty();
            if !self.undo_inserts(applied) {
                // Not all of it could be undone (a persistence failure): the
                // data did change.
                return Err(e);
            }
            // The terms the LOAD minted are unreferenced again: free them
            // now (outside a transaction; inside one at its commit), so the
            // dictionary is back where it was too — part of the no-change.
            self.flush_reclaim();
            // Nothing changed. No cached answer can be keyed by the counts
            // the inserts and their undo went through: the database was not
            // readable in between.
            self.mutations = mutations;
            if let Some(g) = csr {
                self.csr_cache.restore(mutations, g);
            }
            // The inserts and their undo reached a persistent store: make
            // both durable together, as at any commit point.
            if inserted && self.txn.is_none() {
                if let Err(pe) = self.commit_point() {
                    return Err(GStoreError::Database(format!(
                        "{e}; in addition, undoing the failed LOAD could not be persisted: {pe}"
                    )));
                }
            }
            return Err(e);
        }
        drop(csr);
        if skipped > 0 {
            crate::gs_warn!(
                "LOAD <{source}> INTO GRAPH <{}>: {skipped} statement(s) of the document's own \
                 named graphs were not loaded (LOAD without INTO loads them)",
                into.unwrap_or_default()
            );
        }
        Ok(applied.len())
    }

    /// Undo inserts reported by [`insert_quad_id`](Self::insert_quad_id), in
    /// reverse order, through the ordinary delete paths (so the transaction
    /// undo log, the redo log and a persistent store see them too). `false`
    /// when a statement could not be removed (a persistence failure).
    fn undo_inserts(&mut self, applied: Vec<(Option<u32>, IdTriple)>) -> bool {
        let mut all = true;
        // An in-memory store removes the default-graph ones as one batch
        // (the graphs are independent: only the order within one matters).
        let batched = self.store.as_memory().is_some();
        let mut default: Vec<IdTriple> = Vec::new();
        for (graph, id) in applied.into_iter().rev() {
            all &= match graph {
                None if batched => {
                    default.push(id);
                    true
                }
                None => self.remove_default_id(id),
                Some(gid) => self.remove_named_id(gid, id),
            };
        }
        all && self.remove_default_batch(&default) == default.len()
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
    ///
    /// Refused while a transaction is open: the snapshot would hold its
    /// uncommitted writes, and no later increment would undo them when it
    /// rolls back (review db#14).
    pub fn backup<P: AsRef<Path>>(&self, backup_dir: P) -> Result<()> {
        let backup_dir = backup_dir.as_ref();
        if self.txn.is_some() {
            return Err(GStoreError::Database(
                "backup: commit or roll back the open transaction first (a backup taken now \
                 would hold its uncommitted writes, and no later increment would undo them)"
                    .into(),
            ));
        }
        // The reasoning rules first: contract C8 orders a database's
        // rule-directory lock before its persist_lock (review conc#13).
        // They cannot change meanwhile (that takes `&mut self`). A
        // directory another process has open is refused before anything is
        // written into it, as the save below refuses it (review db#11): its
        // lock is taken first (the save shares it).
        fsutil::create_dir_durable(backup_dir)?;
        let _target = dirlock::DirLock::acquire(backup_dir)?;
        self.backup_reason_rules(backup_dir)?;
        let _persist = self.persist_guard();
        self.save_locked(backup_dir)?;
        let off = self.redo_log_len()?;
        write_backup_offset(&backup_dir.join(BACKUP_OFFSET_FILE), off)?;
        self.backup_offset.store(off, Ordering::SeqCst);
        // A new chain starts here: what the logs held before no longer
        // matters to it.
        *self.logs_in_doubt.lock().unwrap_or_else(|p| p.into_inner()) = None;
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
        self.backup_offset.load(Ordering::SeqCst)
    }

    /// The redo log's whole length, or 0 when logging is disabled or the file
    /// does not exist yet. This is the incremental-backup checkpoint unit,
    /// always a commit boundary (see [`logs`]): a torn tail a crash left is
    /// cut off first. Commits still owed to the log are written first; when
    /// that fails they land after this checkpoint later, and replaying them
    /// again over a base that holds them is harmless (redo records make a
    /// triple present or absent; they do not count).
    fn redo_log_len(&self) -> Result<u64> {
        if self.redo_log.is_none() {
            return Ok(0);
        }
        let mut writer = self.redo_writer_lock();
        let _ = writer.flush(LogKind::Redo, true);
        writer.whole_len(LogKind::Redo, 0)
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
    /// * `reason_rule_files/` — the database's reasoning rules and their
    ///   ledgers at the time of the increment (empty when it has none): the
    ///   rules the replayed inferred triples belong to.
    ///
    /// Refused while committed redo records are owed to the log (their
    /// append failed and fails again now): the increment would miss them
    /// (review robust#10). Refused, too, when the log's records since the
    /// last backup do not replay (a corrupt record): such an increment
    /// could never be restored; a full [`backup`](Self::backup) starts a
    /// new chain past the damage.
    pub fn backup_incremental<P: AsRef<Path>>(&self, out_dir: P) -> Result<()> {
        let out_dir = out_dir.as_ref();
        let Some(log_path) = self.redo_log.clone() else {
            return Err(GStoreError::Database(
                "backup_incremental requires redo logging to be enabled \
                 (call enable_redo_log / enable_redo_log_in first)"
                    .into(),
            ));
        };
        // The reasoning rules and ledgers as they are now, which the replayed
        // triples go with (an empty rule directory: none) — taken first, as
        // contract C8 orders a database's rule-directory lock before its
        // persist_lock (review conc#13).
        fsutil::create_dir_durable(out_dir)?;
        self.backup_reason_rules(out_dir)?;
        fs::create_dir_all(out_dir.join(crate::reason::rules::RULE_DIR))?;
        let _persist = self.persist_guard();
        if let Some(why) = self
            .logs_in_doubt
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
        {
            return Err(GStoreError::Database(format!(
                "backup_incremental: the redo log may not match the database since a commit \
                 point failed with an unknown outcome ({why}); take a full backup to start a \
                 new chain"
            )));
        }
        let base = self.backup_offset.load(Ordering::SeqCst);
        let (end, slice) = {
            let mut writer = self.redo_writer_lock();
            if let Err(e) = writer.flush(LogKind::Redo, true) {
                let why = writer
                    .failure(LogKind::Redo)
                    .unwrap_or_else(|| e.to_string());
                return Err(GStoreError::Database(format!(
                    "backup_incremental: {why}; an increment taken now would miss them"
                )));
            }
            // A missing log has no records yet: an increment taken before
            // any logged mutation is an empty (but valid) delta. Any other
            // error reading it is reported, not taken for an empty log.
            let end = writer.whole_len(LogKind::Redo, base)?;
            if end < base {
                return Err(GStoreError::Database(format!(
                    "redo log ({end} bytes) shrank below the backup checkpoint ({base}); \
                     the log was truncated or replaced since the last backup"
                )));
            }
            (end, logs::read_range(&log_path, base, end)?)
        };
        // The copy must replay as `restore_incremental` replays it: the
        // appender's scan steps over a corrupt record of the live log, so
        // a damaged commit would only be found at restore time, and every
        // increment from this one on could never be restored (round-3
        // review). The bytes are in memory already, so checking is cheap.
        if let Err(e) =
            logs::walk(&slice, LogKind::Redo, false, |_| Ok(())).and_then(|w| match w.torn {
                None => Ok(()),
                Some((at, why)) => Err(GStoreError::Database(format!(
                    "redo.log is incomplete at byte {at}: {why}"
                ))),
            })
        {
            return Err(GStoreError::Database(format!(
                "backup_incremental: the redo log since the last backup (bytes [{base}, {end}) \
                 of '{}') cannot be restored — {e} (counted from byte {base}); no increment \
                 was written: take a full backup to start a new chain",
                log_path.display()
            )));
        }
        fsutil::atomic_write(&out_dir.join(INCREMENTAL_LOG_FILE), &slice)?;
        write_incremental_marker(&out_dir.join(INCREMENTAL_MARKER_FILE), base, end)?;
        self.backup_offset.store(end, Ordering::SeqCst);
        Ok(())
    }

    /// Restore from a full base backup and then apply incremental deltas in
    /// order, reconstructing the state captured by the last incremental
    /// (C++ gStore's incremental-restore path).
    ///
    /// `base_dir` is a directory produced by [`backup`](Self::backup); each entry
    /// of `incr_dirs` is a directory produced by
    /// [`backup_incremental`](Self::backup_incremental), in chronological order.
    /// The base (any layout) is read into memory without being written — a
    /// snapshot in place (no lock taken), any other layout through a copy
    /// beside it, in a directory of its own named
    /// `<base>.restore-tmp-<pid>-<seq>-<nonce>` and removed again (review
    /// db#13; the copy needs about the base's size of free space on the
    /// base's file system, or in the system's temporary directory when the
    /// base's parent cannot be written); the backup directories are never
    /// modified — then each delta's `incr.log` is replayed onto it. Deltas are checked for contiguity: each must begin exactly where
    /// the previous one ended (starting from the base's recorded checkpoint), so
    /// an out-of-order or gapped chain is rejected rather than silently
    /// misapplied. The reasoning rules (and ledgers) of the last increment
    /// that carries them (one taken by an earlier version does not) — else
    /// the base's — come with it, held in memory (see
    /// [`into_memory`](Self::into_memory)). Persist the result with
    /// [`save`](Self::save), which writes those rules too.
    pub fn restore_incremental<P: AsRef<Path>, Q: AsRef<Path>>(
        base_dir: P,
        incr_dirs: &[Q],
    ) -> Result<Database> {
        let base_dir = base_dir.as_ref();
        let mut db = Database::read_backup_detached(base_dir)?;
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
                let (base, end) = read_incremental_marker(&dir.join(INCREMENTAL_MARKER_FILE))?;
                if base != expected {
                    return Err(GStoreError::Database(format!(
                        "incremental backup '{}' starts at offset {base} but {expected} \
                         was expected (out-of-order or gapped increment chain)",
                        dir.display()
                    )));
                }
                let bytes = fs::read(dir.join(INCREMENTAL_LOG_FILE))?;
                // The marker's range and the copied records must agree: a
                // shorter copy (even one that ends on a record boundary)
                // would restore less without a word.
                if end.checked_sub(base) != Some(bytes.len() as u64) {
                    return Err(GStoreError::Database(format!(
                        "incremental backup '{}' holds {} log bytes but its marker covers \
                         [{base}, {end})",
                        dir.display(),
                        bytes.len()
                    )));
                }
                // An increment is a whole copy: an incomplete record in it
                // is an error, not a torn tail to skip.
                db.apply_redo_records(&bytes, false)?;
                expected = end;
            }
            // The rules the last increment carries replace the base's.
            if let Some(dir) = incr_dirs
                .iter()
                .map(AsRef::as_ref)
                .rev()
                .find(|d| d.join(crate::reason::rules::RULE_DIR).is_dir())
            {
                db.reason_home.mem = crate::reason::store::read_rule_dir(dir)?;
            }
            Ok(())
        })();
        db.redo_log = saved;
        db.backup_offset.store(expected, Ordering::SeqCst);
        outcome.map(|()| db)
    }

    /// The database in backup directory `dir`, in memory, read without
    /// writing `dir` (review db#13): a snapshot is read in place without
    /// taking its lock (as [`read_copy`](Self::read_copy)); any other layout
    /// is copied into a scratch directory, opened and detached from there,
    /// and the copy removed. The reasoning rules (and ledgers) come along,
    /// held in memory.
    ///
    /// The scratch directory is created beside `dir` (on its file system:
    /// the copy needs about the base's size of free space there, besides
    /// the memory the restored database takes), or in the system's
    /// temporary directory when `dir`'s parent cannot be written. It is
    /// created exclusively under a fresh name, mode `0700` (round-3 review:
    /// a predictable name in the shared temporary directory was removed and
    /// recreated, so a directory or symlink another user had put there was
    /// written into), and only a directory this call created is removed.
    fn read_backup_detached(dir: &Path) -> Result<Database> {
        match Database::storage_kind(dir) {
            Some(StorageKind::Snapshot) => {
                let mut db = Database::read_copy(dir)?;
                db.reason_home.mem = crate::reason::store::read_rule_dir(dir)?;
                return Ok(db);
            }
            Some(_) => {}
            None => {
                return Err(GStoreError::Database(format!(
                    "'{}' does not contain a gStore database",
                    dir.display()
                )))
            }
        }
        /// The scratch copy, removed when dropped (also on failure).
        struct Scratch(PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        let base_name = dir
            .file_name()
            .map_or_else(|| "backup".into(), |n| n.to_string_lossy().into_owned());
        let stem = format!(
            "{base_name}.restore-tmp-{}-{}-{nonce:x}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let beside = match dir.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        let created = match create_private_dir(&beside, &stem) {
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
                ) =>
            {
                create_private_dir(&std::env::temp_dir(), &stem)
            }
            other => other,
        };
        let scratch = Scratch(created.map_err(|e| {
            GStoreError::Database(format!(
                "reading backup '{}': cannot create a scratch directory beside it or in the \
                 temporary directory: {e}",
                dir.display()
            ))
        })?);
        Database::backup_dir(dir, &scratch.0)
            .and_then(|()| Database::open(&scratch.0)?.into_memory())
            .map_err(|e| {
                GStoreError::Database(format!(
                    "reading backup '{}' through a copy in '{}': {e}",
                    dir.display(),
                    scratch.0.display()
                ))
            })
    }

    // ---- update log -------------------------------------------------------

    /// Enable update logging to `path`: every subsequent SPARQL UPDATE applied
    /// through [`query`](Self::query) is appended to the file (gStore
    /// `update.log`) once it is committed — after its commit point, or at the
    /// commit of the transaction it ran in (one rolled back is never
    /// logged). The file is created on first write; its format and crash
    /// behaviour are those of the redo log (see
    /// [`enable_redo_log`](Self::enable_redo_log)).
    pub fn enable_update_log<P: AsRef<Path>>(&mut self, path: P) {
        let path = path.as_ref().to_path_buf();
        self.update_writer_mut().retarget(&path, LogKind::Update);
        self.update_log = Some(path);
    }

    /// Enable update logging at the conventional `update.log` inside a database
    /// directory (created if necessary).
    pub fn enable_update_log_in<P: AsRef<Path>>(&mut self, dir: P) -> Result<()> {
        let dir = dir.as_ref();
        fs::create_dir_all(dir)?;
        self.enable_update_log(dir.join(UPDATE_LOG_FILE));
        Ok(())
    }

    /// Disable update logging (subsequent updates are not recorded). The open
    /// transaction's statements are not logged; committed ones whose append
    /// failed are written now (when that fails again, they are kept for the
    /// next time this file is enabled).
    pub fn disable_update_log(&mut self) {
        self.update_pending.clear();
        let _ = self.update_writer_mut().flush(LogKind::Update, true);
        self.update_log = None;
    }

    /// Whether update logging is currently enabled.
    pub fn update_log_enabled(&self) -> bool {
        self.update_log.is_some()
    }

    /// Replay every UPDATE statement recorded in an update log `path` against
    /// this database, in order, returning the number of statements applied.
    /// Logging is suspended during replay so the log is not rewritten and no
    /// recursion occurs. A torn tail a crash left (see
    /// [`replay_redo_log`](Self::replay_redo_log)) is skipped; a damaged
    /// record anywhere else is an error.
    ///
    /// A record is a request that succeeded when it was logged, possibly by
    /// an earlier version whose rules were more lenient. Where this version
    /// refuses what those accepted, the record is read as they read it: a
    /// labelled blank node in `DELETE DATA` / `DELETE WHERE` / a DELETE
    /// template names the stored node, `CREATE` / `DROP` / `CLEAR GRAPH`
    /// are `SILENT` (a record of a missing graph was a no-op when logged),
    /// and the grammar leniencies of earlier parsers (a missing `.` between
    /// triple statements, `PREFIX p:x <…>`, a repeated `LIMIT`) are
    /// accepted. Requests are evaluated by this version: a function whose
    /// result changed (`SECONDS` returns an `xsd:decimal`) gives its new
    /// result. The first record that still fails stops the replay with its
    /// error; the records before it stay applied.
    pub fn replay_update_log<P: AsRef<Path>>(&mut self, path: P) -> Result<usize> {
        let data = fs::read(path)?;
        let saved = self.update_log.take();
        let result = self.replay_records(&data);
        self.update_log = saved;
        result
    }

    /// Parse and apply the records of an update log buffer, a commit at a
    /// time (see [`logs`]).
    fn replay_records(&mut self, bytes: &[u8]) -> Result<usize> {
        let mut applied = 0usize;
        let walk = logs::walk(bytes, LogKind::Update, false, |commit| {
            let mut statements = Vec::with_capacity(commit.len());
            for r in commit {
                statements.push(std::str::from_utf8(r.body).map_err(|_| {
                    GStoreError::Database(format!(
                        "update.log record at byte {} is not UTF-8",
                        r.start
                    ))
                })?);
            }
            for sparql in statements {
                // Read as the version that logged it read it (round-4
                // review: records earlier versions accepted — a DROP GRAPH
                // of a missing graph, a labelled blank node in DELETE DATA
                // — stopped the replay at the first one).
                self.execute_as(Statement::parse_logged(sparql)?, None, None)?;
                applied += 1;
            }
            Ok(())
        })?;
        if let Some((at, why)) = walk.torn {
            crate::gs_warn!("update.log: skipping a torn tail at byte {at} ({why})");
        }
        Ok(applied)
    }

    /// Append the open transaction's (or the ending statement's) update-log
    /// records, as one commit (see [`logs`]).
    fn flush_update_log(&mut self, sync: bool) -> Result<()> {
        let recs = std::mem::take(&mut self.update_pending);
        let Some(path) = self.update_log.clone() else {
            return Ok(());
        };
        let mut commit = Vec::new();
        for (i, r) in recs.iter().enumerate() {
            logs::frame_update(
                &mut commit,
                r.millis,
                r.changed,
                &r.sparql,
                i + 1 == recs.len(),
            );
        }
        self.update_writer_mut()
            .append(&path, LogKind::Update, &commit, sync)
    }

    fn update_writer_mut(&mut self) -> &mut logs::LogWriter {
        self.update_writer
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn update_writer_lock(&self) -> MutexGuard<'_, logs::LogWriter> {
        self.update_writer.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- redo log (the redo half of gStore's WAL) -------------------------

    /// Enable redo logging to `path`: every subsequent *committed* triple
    /// mutation (insert/delete, default or named graph, inferred triples,
    /// clears) is appended as a redo record so it can be re-applied on
    /// recovery with [`replay_redo_log`](Self::replay_redo_log), or carried
    /// by [`backup_incremental`](Self::backup_incremental).
    ///
    /// The records of one commit are appended with one write, after its
    /// commit point: an auto-commit UPDATE's at its end (`fdatasync`ed), a
    /// transaction's at [`commit`](Self::commit) (`fdatasync`ed; one rolled
    /// back is never logged), a direct-API mutation's
    /// ([`insert_triple`](Self::insert_triple), …, which has no commit point
    /// of its own) when it is applied, `fdatasync`ed by the next commit
    /// point ([`sync`](Self::sync), an UPDATE, a commit, a backup). An
    /// append that fails is cut off again and its records are kept, in
    /// order, for the next append: an UPDATE or a commit reports it,
    /// [`sync`](Self::sync) too, and
    /// [`backup_incremental`](Self::backup_incremental) refuses until the
    /// log has them. A crash in the middle of an append leaves a torn tail,
    /// which replay skips (a commit replays whole or not at all) and the
    /// next append cuts off. The record format is in the `logs` module docs:
    /// length-prefixed, with a CRC-32 per record; logs written before
    /// checksums replay as before.
    pub fn enable_redo_log<P: AsRef<Path>>(&mut self, path: P) {
        let path = path.as_ref().to_path_buf();
        self.redo_writer_mut().retarget(&path, LogKind::Redo);
        self.redo_log = Some(path);
    }

    /// Enable redo logging at the conventional `redo.log` inside a database
    /// directory (created if necessary).
    pub fn enable_redo_log_in<P: AsRef<Path>>(&mut self, dir: P) -> Result<()> {
        let dir = dir.as_ref();
        fs::create_dir_all(dir)?;
        self.enable_redo_log(dir.join(REDO_LOG_FILE));
        Ok(())
    }

    /// Disable redo logging (subsequent mutations are not recorded). The open
    /// transaction's buffered records are dropped; committed ones whose
    /// append failed are written now (when that fails again, they are kept
    /// for the next time this file is enabled — never dropped, review db#2).
    pub fn disable_redo_log(&mut self) {
        self.redo_pending.clear();
        let _ = self.redo_writer_mut().flush(LogKind::Redo, true);
        self.redo_log = None;
    }

    /// Whether redo logging is currently enabled.
    pub fn redo_log_enabled(&self) -> bool {
        self.redo_log.is_some()
    }

    /// Record one applied mutation for the redo log. Inside a (single-writer)
    /// transaction, or an auto-commit statement ([`redo_hold`](Self::redo_hold)),
    /// it is buffered until the commit; a direct-API mutation is committed
    /// as it is applied, so its record is appended now. No-op when the redo
    /// log is disabled. A failed append cannot change a mutation's `bool`
    /// result: its record is owed to the log (see
    /// [`enable_redo_log`](Self::enable_redo_log)).
    fn record_redo(&mut self, op: u8, graph: Option<u32>, id: IdTriple) {
        if self.redo_log.is_none() {
            return;
        }
        let (Some(s), Some(p), Some(o)) = (
            self.dict.term(id.sub),
            self.dict.pred(id.pred),
            self.dict.term(id.obj),
        ) else {
            return;
        };
        // Each term exactly as stored, in the `record_key` form
        // `parse_record_triple` splits unambiguously (an IRI's excluded
        // characters escaped, as before; a label or language tag the strict
        // readers refuse escaped too), so replay names the stored terms,
        // odd ones included (review parser#1).
        let triple = format!("{} {} {} .", record_key(&s), record_key(&p), record_key(&o));
        let graph = graph
            .and_then(|g| self.dict.term(g))
            .map(|g| strip_angle(&g).to_owned());
        self.redo_pending.push(RedoRec { op, graph, triple });
        if self.txn.is_none() && !self.redo_hold {
            let _ = self.flush_redo(false);
        }
    }

    /// Append the buffered redo records as one commit (`fdatasync`ed when
    /// `sync`), after any commit still owed to the log.
    fn flush_redo(&mut self, sync: bool) -> Result<()> {
        let recs = std::mem::take(&mut self.redo_pending);
        let Some(path) = self.redo_log.clone() else {
            return Ok(());
        };
        let mut commit = Vec::new();
        for (i, r) in recs.iter().enumerate() {
            logs::frame_redo(
                &mut commit,
                r.op,
                r.graph.as_deref(),
                &r.triple,
                i + 1 == recs.len(),
            );
        }
        self.redo_writer_mut()
            .append(&path, LogKind::Redo, &commit, sync)
    }

    /// Run `body`, one direct-API call that may apply many mutations
    /// ([`insert_sources`](Self::insert_sources),
    /// [`try_insert_data`](Self::try_insert_data), …), as one redo-log
    /// commit: its records are appended together when it returns — also
    /// when it fails or panics part-way, since what it applied stays —
    /// instead of one append per triple (review db#8). Like any direct-API
    /// write it is `fdatasync`ed by the next commit point. Inside a
    /// transaction or a statement (whose commit appends the records), or
    /// with no redo log, `body` just runs.
    fn redo_batch<T>(&mut self, body: impl FnOnce(&mut Self) -> T) -> T {
        if self.redo_log.is_none() || self.redo_hold || self.txn.is_some() {
            return body(self);
        }
        self.redo_hold = true;
        let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(self)));
        self.redo_hold = false;
        // A failed append is owed (see `enable_redo_log`).
        let _ = self.flush_redo(false);
        done.unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    }

    /// The commit that just succeeded: append its redo records and its
    /// statements' update-log records, each log in one `fdatasync`ed
    /// append.
    fn log_commit(&mut self) -> Result<()> {
        let redo = self.flush_redo(true);
        let update = self.flush_update_log(true);
        redo.and(update)
    }

    /// The end of an auto-commit UPDATE request (`update`: its text and how
    /// many triples it changed, `None` when it failed part-way), after its
    /// commit point. Inside a transaction its update-log record waits for
    /// the commit (its redo records already do). Outside one its records
    /// follow `fate`, what the commit point left (round-3 review: the logs
    /// follow what a reopen keeps, on every layout and for transactions
    /// alike): appended when it succeeded or when the store's write-ahead
    /// log holds the whole request (the next open replays it); dropped when
    /// nothing of it reached the store; dropped with the logs marked in
    /// doubt when its outcome is unknown (an on-disk store that failed
    /// part-way, a RocksDB journal that could not be deleted or a log sync
    /// that failed).
    fn end_statement_logs(
        &mut self,
        update: Option<(&str, usize)>,
        fate: CommitFate,
    ) -> Result<()> {
        self.redo_hold = false;
        if self.redo_log.is_none() && self.update_log.is_none() {
            return Ok(());
        }
        if let (Some((sparql, changed)), true) = (update, self.update_log.is_some()) {
            self.update_pending.push(UpdateRec::now(sparql, changed));
        }
        if self.txn.is_some() {
            return Ok(());
        }
        match fate {
            CommitFate::Durable | CommitFate::Replayed => self.log_commit(),
            CommitFate::NotApplied | CommitFate::InDoubt => {
                self.redo_pending.clear();
                self.update_pending.clear();
                if fate == CommitFate::InDoubt {
                    let why = self
                        .storage_failure()
                        .unwrap_or_else(|| "a commit point failed".into());
                    self.note_logs_in_doubt(&GStoreError::Database(why));
                }
                Ok(())
            }
        }
    }

    /// The commit point of the logs, for [`sync`](Self::sync): append what
    /// is still owed to each enabled log and `fdatasync` it.
    pub(super) fn sync_logs(&self) -> Result<()> {
        let redo = match self.redo_log {
            Some(_) => self.redo_writer_lock().flush(LogKind::Redo, true),
            None => Ok(()),
        };
        let update = match self.update_log {
            Some(_) => self.update_writer_lock().flush(LogKind::Update, true),
            None => Ok(()),
        };
        redo.and(update)
    }

    fn redo_writer_mut(&mut self) -> &mut logs::LogWriter {
        self.redo_writer
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn redo_writer_lock(&self) -> MutexGuard<'_, logs::LogWriter> {
        self.redo_writer.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Re-apply every mutation recorded in a redo log `path` against this
    /// database, in order, returning the number of records applied. Replay onto a
    /// database in the same state the log began from reconstructs the final
    /// state. Redo logging is suspended during replay so the log is not rewritten.
    ///
    /// A commit is applied only once all of its records were read and
    /// checked; a torn tail a crash left (an incomplete last commit) is
    /// skipped. A damaged record anywhere else, an unknown op or a record
    /// without its triple is an error (review gap-doc-claims#1, db#9).
    pub fn replay_redo_log<P: AsRef<Path>>(&mut self, path: P) -> Result<usize> {
        let data = fs::read(path)?;
        let saved = self.redo_log.take();
        let result = self.apply_redo_records(&data, true);
        self.redo_log = saved;
        result
    }

    /// Parse and apply the records of a redo log buffer, a commit at a time.
    /// `torn_ok`: a torn tail is skipped (a live log after a crash) rather
    /// than an error (an increment, which is a whole copy).
    fn apply_redo_records(&mut self, bytes: &[u8], torn_ok: bool) -> Result<usize> {
        let mut applied = 0usize;
        let walk = logs::walk(bytes, LogKind::Redo, false, |commit| {
            // Every record of the commit is checked before any is applied.
            let ops = commit
                .iter()
                .map(parse_redo_record)
                .collect::<Result<Vec<_>>>()?;
            for op in ops {
                self.apply_redo_op(op)?;
                applied += 1;
            }
            Ok(())
        })?;
        if let Some((at, why)) = walk.torn {
            if !torn_ok {
                return Err(GStoreError::Database(format!(
                    "redo records end incomplete at byte {at} ({why})"
                )));
            }
            crate::gs_warn!("redo.log: skipping a torn tail at byte {at} ({why})");
        }
        Ok(applied)
    }

    /// Apply one checked redo record.
    fn apply_redo_op(&mut self, op: RedoOp<'_>) -> Result<()> {
        match op {
            RedoOp::Insert(graph, triples) => {
                for t in triples {
                    self.insert_quad(&t, graph)?;
                }
            }
            RedoOp::Delete(graph, triples) => {
                for t in triples {
                    self.delete_quad(&t, graph);
                }
            }
            // CLEAR / DROP (and the clears of MOVE / COPY).
            RedoOp::Clear(None) => {
                self.clear_default()?;
            }
            RedoOp::Clear(Some(g)) => {
                self.clear_named(g);
            }
            RedoOp::ClearNamed => {
                self.clear_all_named();
            }
        }
        self.flush_reclaim();
        Ok(())
    }

    // (Id reclamation lives in the `reclaim` module.)

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
    ///
    /// A read error of the store (Stream mode, RocksDB) ends the run early
    /// and is only logged: [`try_run_rules`](Self::try_run_rules) reports it.
    pub fn run_rules(&mut self) -> usize {
        let (n, error) = self.run_rules_scoped();
        if let Some(e) = error {
            crate::gs_warn!("running the reasoning rules of '{}': {e}", self.name);
        }
        n
    }

    /// [`run_rules`](Self::run_rules), reporting a read error of the store
    /// as [`try_materialize_rdfs`](Self::try_materialize_rdfs) does (review
    /// design#7); the triples inferred before it stay inserted.
    pub fn try_run_rules(&mut self) -> Result<usize> {
        match self.run_rules_scoped() {
            (n, None) => Ok(n),
            (_, Some(e)) => Err(e),
        }
    }

    /// The run of [`run_rules`](Self::run_rules): the number of triples
    /// inferred, and the read error that ended it early.
    fn run_rules_scoped(&mut self) -> (usize, Option<GStoreError>) {
        let mut rules = std::mem::take(&mut self.rules);
        let out = self.infer_scoped("running the reasoning rules", |dict, store, insert| {
            rules.apply_with(dict, store, insert)
        });
        self.rules = rules;
        out
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
    /// backup, and [`save`](Self::save) into the saved directory when there
    /// are any (open the saved directory, or
    /// [`set_reason_db_dir`](Self::set_reason_db_dir), to keep managing them
    /// there).
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
        self.with_rule_store(|db, store| {
            crate::reason::manage::disable(db, store, name, None, |db| db.persist_for_rules())
        })
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

    /// Make the data durable where this database's rules (and their
    /// ledgers) live, before a disable empties a ledger: a snapshot database
    /// whose rules are kept in its [`home`](Self::home) is saved there; the
    /// persistent layouts are at a commit point already ([`sync`](Self::sync)).
    fn persist_for_rules(&self) -> Result<()> {
        if self.storage() == crate::StorageKind::Snapshot {
            if let Some(home) = self.home.clone() {
                if self
                    .reason_db_dir()
                    .is_some_and(|d| fsutil::same_dir(&d, &home))
                {
                    return self.save(&home);
                }
            }
        }
        self.sync()
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
    /// of them existed before, and which it added. The WHERE clause is
    /// evaluated **once** and exactly the triples that evaluation derived are
    /// inserted (as `INSERT DATA`; the update log records `sparql`), so a
    /// filter that is not deterministic (`RAND()`, `NOW()`, …) can never
    /// insert a triple the ledger does not record. An error before anything
    /// was applied (parse, WHERE evaluation, cancellation) is `Err`; the
    /// insert's own outcome is the report's `result`.
    ///
    /// `record` is called with every derived triple's ledger line and
    /// whether it exists, after the evaluation and before anything is
    /// inserted (the rule records what it is about to insert — see
    /// [`crate::reason::manage::execute`]); when it fails nothing is
    /// inserted and its message is the error.
    pub(crate) fn reason_infer(
        &mut self,
        sparql: &str,
        cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
        record: impl FnOnce(&[(String, bool)]) -> std::result::Result<(), String>,
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
        // The static checks the update path runs before applying anything.
        {
            let mut eval = Evaluator::new(&self.dict, &self.store);
            if !self.functions.is_empty() {
                eval = eval.with_functions(self.functions.clone());
            }
            eval.check_update_calls(&ops)?;
        }
        let (_, ins) = self.modify_changes(
            delete,
            insert,
            pattern,
            None,
            using,
            using_named,
            cancel.as_ref(),
            None,
        )?;
        let mut seen = HashSet::new();
        let mut derived: Vec<(String, bool, Triple)> = Vec::new();
        // Whether each derived triple existed is read in a read scope (C2;
        // review design#7): a store that fails a read must fail the run
        // before the ledger records a wrong answer.
        let scope = ReadScope::enter();
        for (g, t) in &ins {
            if g.is_some() {
                continue;
            }
            let line = ledger_line(t);
            if seen.insert(line.clone()) {
                let existed = self.contains_triple(t);
                derived.push((line, existed, t.clone()));
            }
        }
        if let Some(e) = scope.take_error() {
            return Err(read_failure("checking which derived triples exist", e));
        }
        drop(scope);
        let lines: Vec<(String, bool)> = derived.iter().map(|(l, e, _)| (l.clone(), *e)).collect();
        record(&lines).map_err(GStoreError::Database)?;
        let ground = ins
            .into_iter()
            .map(|(graph, t)| GroundTriple {
                subject: t.subject,
                predicate: t.predicate,
                object: t.object,
                graph,
            })
            .collect();
        let result = match self.run_update(sparql, vec![UpdateOp::InsertData(ground)], cancel, None)
        {
            Ok(QueryResult::Update { changed }) => Ok(changed),
            Ok(_) => Ok(0),
            Err(e) => Err(e.to_string()),
        };
        // Which of them the insert added, in a read scope: when a read
        // fails, every derived triple that did not exist is reported, so
        // the rule owns (and its disable retracts) all it may have added —
        // one it did not add is simply absent then.
        let scope = ReadScope::enter();
        let mut inserted: Vec<String> = derived
            .iter()
            .filter(|(_, existed, t)| !existed && self.contains_triple(t))
            .map(|(line, _, _)| line.clone())
            .collect();
        if scope.take_error().is_some() {
            inserted = derived
                .iter()
                .filter(|(_, existed, _)| !existed)
                .map(|(line, _, _)| line.clone())
                .collect();
        }
        drop(scope);
        Ok(InferReport {
            derived: lines,
            inserted,
            result,
        })
    }

    /// Retract the triples of ledger `lines` (see
    /// [`crate::reason::manage`]) as one `DELETE DATA` through the update
    /// path. Returns how many were removed; fails, removing nothing, when a
    /// line cannot be read back. The opt-in statement log
    /// (`update.log`) records that `DELETE DATA` text; a blank node in it is
    /// written as its stored label, which SPARQL does not accept in `DELETE
    /// DATA` but [`replay_update_log`](Self::replay_update_log) reads as the
    /// stored node again.
    pub(crate) fn reason_retract(
        &mut self,
        lines: &[String],
        cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<usize> {
        let mut ground = Vec::with_capacity(lines.len());
        let mut text = String::from("DELETE DATA {");
        for line in lines {
            // A line that cannot be read back fails the whole retraction
            // (nothing is deleted, the ledger stays): skipping it would
            // leave its triple behind with no rule owning it.
            let Some(t) = crate::reason::manage::parse_ledger_line(line) else {
                let shown: String = line.escape_debug().take(200).collect();
                return Err(GStoreError::Database(format!(
                    "the rule's ledger holds a line that cannot be read back (\"{shown}\"); \
                     nothing was retracted"
                )));
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
        match self.run_update(&text, vec![UpdateOp::DeleteData(ground)], cancel, None)? {
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
        match self.run_update(sparql, ops, cancel, None)? {
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
        *self.query_cache_mut() = QueryCache::new(bytes);
    }

    /// The read-query cache's budget in bytes (see
    /// [`set_query_cache_size`](Self::set_query_cache_size)).
    pub fn query_cache_size(&self) -> usize {
        self.query_cache().budget()
    }

    /// Number of results currently in the read-query cache.
    pub fn query_cache_len(&self) -> usize {
        self.query_cache().len()
    }

    /// Approximate bytes the read-query cache currently holds.
    pub fn query_cache_bytes(&self) -> usize {
        self.query_cache().bytes()
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
            self.query_cache_mut().clear();
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
            self.query_cache_mut().clear();
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
            self.query_cache_mut().clear();
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
        self.query_cache().contains(sparql)
    }

    // ---- bulk / parallel loaders -----------------------------------------

    /// Build a database from an N-Triples file with bounded transient memory
    /// (gStore RDFParser's chunked import): the file is streamed and each
    /// triple is encoded to ids as it is read — never more than one triple's
    /// terms are held as text — so it suits files too large to buffer
    /// whole. The encoded triples (12 bytes each, about half of what the
    /// store will hold for them) are indexed in one bulk load at the end, in
    /// time linear in the file: loading every `batch` triples into the
    /// growing store re-sorted the whole store each time, quadratic in the
    /// file (review db#15). `batch` now only sizes the first allocation of
    /// the id buffer (at most 2^20 entries); it is kept for compatibility.
    pub fn build_from_ntriples_batched<P: AsRef<Path>>(
        name: impl Into<String>,
        path: P,
        batch: usize,
    ) -> Result<Database> {
        let mut db = Database::new(name);
        let mut ids: Vec<IdTriple> = Vec::with_capacity(batch.clamp(1, 1 << 20));
        crate::parser::ntriples::for_each_triple_file(path, |t| {
            ids.push(db.encode_triple(&t)?);
            Ok(())
        })?;
        db.store.bulk_load(ids);
        db.rebuild_index();
        Ok(db)
    }

    /// Build a database from an N-Triples file using `threads` parser threads
    /// (gStore's multi-threaded load). The file streams in rounds of
    /// `threads` line-aligned chunks of about 1 MiB, each round parsed in
    /// parallel and then interned serially (the dictionary is the single
    /// synchronization point) — memory holds one round of text, not the file.
    /// Indexing runs once at the end.
    pub fn build_from_ntriples_parallel<P: AsRef<Path>>(
        name: impl Into<String>,
        path: P,
        threads: usize,
    ) -> Result<Database> {
        use std::io::BufRead;
        const CHUNK_BYTES: usize = 1 << 20;
        let threads = threads.max(1);
        let mut reader = std::io::BufReader::with_capacity(
            crate::parser::stream::DEFAULT_WINDOW,
            fs::File::open(path)?,
        );
        let mut db = Database::new(name);
        let mut ids: Vec<IdTriple> = Vec::new();
        let mut lines_before = 0usize;
        loop {
            // One round: up to `threads` chunks, each ending at a line break,
            // with the number of lines that precede it (for error lines).
            let mut chunks: Vec<(usize, String)> = Vec::with_capacity(threads);
            for _ in 0..threads {
                let mut buf = Vec::new();
                let mut lines = 0usize;
                while buf.len() < CHUNK_BYTES && reader.read_until(b'\n', &mut buf)? > 0 {
                    lines += 1;
                }
                if buf.is_empty() {
                    break;
                }
                let text = String::from_utf8(buf).map_err(|_| GStoreError::RdfParse {
                    line: lines_before + 1,
                    msg: "invalid UTF-8 in this chunk of the file".into(),
                })?;
                chunks.push((lines_before, text));
                lines_before += lines;
            }
            if chunks.is_empty() {
                break;
            }
            let parsed: Vec<Result<Vec<Triple>>> = std::thread::scope(|scope| {
                let handles: Vec<_> = chunks
                    .iter()
                    .map(|(before, text)| {
                        scope.spawn(move || {
                            crate::parser::ntriples::parse_str(text).map_err(|e| match e {
                                GStoreError::RdfParse { line, msg } => GStoreError::RdfParse {
                                    line: line + before,
                                    msg,
                                },
                                other => other,
                            })
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .map(|h| h.join().expect("parser thread panicked"))
                    .collect()
            });
            for chunk in parsed {
                for t in chunk? {
                    ids.push(db.encode_triple(&t)?);
                }
            }
        }
        db.store.bulk_load(ids);
        db.rebuild_index();
        Ok(db)
    }

    /// Parallel-parse an in-memory N-Triples document with `threads` threads,
    /// then intern + index serially. See
    /// [`build_from_ntriples_parallel`](Self::build_from_ntriples_parallel).
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

/// `Database` may be shared by reference across threads: its `&self` methods
/// (reads, `save`, `sync`, backups) may run concurrently, its `&mut self`
/// methods need exclusive access (contract C8).
#[allow(dead_code)]
fn assert_send_sync<T: Send + Sync>() {}
const _: fn() = || assert_send_sync::<Database>();

/// The error a statement fails with when a read inside it reported `msg`
/// through [`note_read_error`](crate::store::read_scope::note_read_error).
fn read_failure(what: &str, msg: String) -> GStoreError {
    GStoreError::Database(format!("a read from the store failed while {what}: {msg}"))
}

/// The named-graph IRI of an imported quad (`None` = the default graph). The
/// import pipeline has already turned a blank-node label into an IRI.
fn graph_iri(graph: &Option<Term>) -> Option<String> {
    match graph {
        Some(Term::Iri(g)) => Some(g.clone()),
        Some(Term::Blank(l)) => Some(format!("{}{l}", import::BNODE_GRAPH_PREFIX)),
        Some(Term::Literal { .. }) | None => None,
    }
}

/// Collects a streaming build: quads are encoded batch by batch; the default
/// graph's ids (12 bytes a triple) are bulk-loaded once at the end, named
/// graphs per graph.
#[derive(Default)]
pub(crate) struct BuildAccumulator {
    default: Vec<IdTriple>,
    named: HashMap<u32, Vec<IdTriple>>,
}

impl BuildAccumulator {
    /// Encode a batch of quads into `db`'s dictionary.
    pub(crate) fn add(&mut self, db: &mut Database, batch: &mut Vec<Quad>) -> Result<()> {
        for q in batch.drain(..) {
            let graph = graph_iri(&q.graph);
            let id = db.encode_triple(&q.into_triple())?;
            match graph {
                None => self.default.push(id),
                Some(g) => {
                    let gid = db.dict.try_intern_entity(&Term::iri(g).dict_key())?;
                    self.named.entry(gid).or_default().push(id);
                }
            }
        }
        Ok(())
    }

    /// Load the collected ids into `db` and build its index.
    pub(crate) fn finish(self, db: &mut Database) {
        db.store.bulk_load(self.default);
        for (gid, ids) in self.named {
            db.named.bulk_load(gid, ids);
        }
        db.rebuild_index();
    }
}

/// One checked redo record (see [`parse_redo_record`]); the graph is a
/// named graph's IRI, `None` the default graph.
enum RedoOp<'a> {
    Insert(Option<&'a str>, Vec<Triple>),
    Delete(Option<&'a str>, Vec<Triple>),
    /// `CLEAR` of the default graph (`None`) or of one named graph.
    Clear(Option<&'a str>),
    /// `CLEAR NAMED`.
    ClearNamed,
}

/// Check a redo record and read its op: `I` / `D` with at least one triple,
/// `C` / `N` with the empty body of the clears written before
/// [`CLEAR_BODY`] or that body (`N` without a graph). Any other op is
/// refused before its body is read — a record a later version adds must
/// not replay as nothing (review gap-doc-claims#1).
fn parse_redo_record<'a>(r: &logs::Record<'a>) -> Result<RedoOp<'a>> {
    let at = r.start;
    let op = r.fields.first().copied().unwrap_or("");
    if !matches!(op, "I" | "D" | "C" | "N") {
        return Err(GStoreError::Database(format!(
            "unknown redo.log op '{op}' at byte {at}"
        )));
    }
    let glen: usize = r.fields.get(1).and_then(|f| f.parse().ok()).unwrap_or(0);
    let named = r.fields.get(3) == Some(&"G");
    let (graph, triple) = r.body.split_at(glen.min(r.body.len()));
    let text = |b: &'a [u8], what: &str| {
        std::str::from_utf8(b).map_err(|_| {
            GStoreError::Database(format!("redo.log record at byte {at}: non-UTF-8 {what}"))
        })
    };
    let (graph, triple) = (text(graph, "graph")?, text(triple, "triple")?);
    // A named-graph record is flagged `G`; without the flag (a log
    // written before it existed) a non-empty graph is named.
    let graph = (named || !graph.is_empty()).then_some(graph);
    match op {
        "C" | "N" => {
            if !(triple.is_empty() || triple == CLEAR_BODY) || (op == "N" && graph.is_some()) {
                return Err(GStoreError::Database(format!(
                    "malformed redo.log clear record at byte {at}"
                )));
            }
            Ok(if op == "C" {
                RedoOp::Clear(graph)
            } else {
                RedoOp::ClearNamed
            })
        }
        _ => {
            // The record names stored terms (verbatim blank labels), in
            // `record_key` form: read with the matching parser, so a term
            // the strict N-Triples reader refuses (a language tag `zh_CN`,
            // a label holding a space) replays as itself (review parser#1).
            // A record of another shape is read as N-Triples, as before.
            let triples = match crate::model::term::parse_record_triple(triple) {
                Some(t) => vec![t],
                None => crate::parser::ntriples::parse_str(triple)?,
            };
            if triples.is_empty() {
                return Err(GStoreError::Database(format!(
                    "redo.log {op} record at byte {at} names no triple"
                )));
            }
            Ok(if op == "I" {
                RedoOp::Insert(graph, triples)
            } else {
                RedoOp::Delete(graph, triples)
            })
        }
    }
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
            return it.next().and_then(|s| s.parse().ok()).ok_or_else(|| {
                GStoreError::Database(format!("marker field '{key}' is not a u64"))
            });
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

/// Create a new directory `<parent>/<stem>` — or `<stem>-1`, `<stem>-2`, …
/// when that name exists — exclusively (`mkdir`, which neither reuses an
/// existing directory nor follows a symlink) and, on Unix, with mode
/// `0700`. An existing entry of any of these names is left alone.
fn create_private_dir(parent: &Path, stem: &str) -> std::io::Result<PathBuf> {
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    for attempt in 0u32..1000 {
        let path = match attempt {
            0 => parent.join(stem),
            n => parent.join(format!("{stem}-{n}")),
        };
        match builder.create(&path) {
            Ok(()) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!("every name '{stem}-N' under '{}' exists", parent.display()),
    ))
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

/// Remove a pager-backed file and its sibling `<path>.wal`, ignoring only
/// absence ([`Pager::remove_with_wal`](crate::kvstore::Pager::remove_with_wal)).
/// Used before a fresh disk build so a stale WAL from a crashed prior build can
/// never replay into the new file (see [`crate::kvstore::pager`] recovery):
/// a log that cannot be removed fails the build (review disk#9: every error
/// used to be ignored).
fn remove_file_and_wal(path: &Path) -> Result<()> {
    crate::kvstore::Pager::remove_with_wal(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Term;
    use crate::query::QueryResult;

    /// Round-3 review: the restore's scratch directory is made by `mkdir`
    /// under a name nobody holds — an existing directory (with someone
    /// else's file) or a symlink of that name is skipped, never reused,
    /// followed or removed — with mode 0700.
    #[test]
    fn a_private_dir_is_created_exclusively() {
        let parent =
            std::env::temp_dir().join(format!("gstore-private-dir-test-{}", std::process::id()));
        struct Rm(PathBuf);
        impl Drop for Rm {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let _rm = Rm(parent.clone());
        let _ = fs::remove_dir_all(&parent);
        fs::create_dir_all(parent.join("stem")).unwrap();
        fs::write(parent.join("stem").join("theirs"), b"x").unwrap();
        fs::create_dir_all(parent.join("elsewhere")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(parent.join("elsewhere"), parent.join("stem-1")).unwrap();
        #[cfg(not(unix))]
        fs::create_dir_all(parent.join("stem-1")).unwrap();
        let made = create_private_dir(&parent, "stem").unwrap();
        assert_eq!(made, parent.join("stem-2"));
        assert_eq!(
            fs::read_dir(&made).unwrap().count(),
            0,
            "a new, empty directory"
        );
        assert!(parent.join("stem").join("theirs").exists());
        assert_eq!(fs::read_dir(parent.join("elsewhere")).unwrap().count(), 0);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&made).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700);
        }
    }

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
            assert!(inc.insert_triple(&Triple::new(Term::iri(s), Term::iri(p), Term::iri(o))));
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
            assert!(inc.insert_triple(&Triple::new(Term::iri(&s), Term::iri("p"), Term::iri(&o))));
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

    /// A delete leaves the index valid (its entries stay sound supersets) and
    /// counts one unit of shrink debt; queries return the correct results
    /// through the tree, and a rebuild clears the debt. (Before Phase 6 a
    /// delete invalidated the index.)
    #[test]
    fn delete_keeps_the_index_valid_and_results_correct() {
        let mut db = Database::build_from_str("d", SMALL).unwrap();
        assert!(db.stats().index_valid);
        assert!(db.remove_triple(&Triple::new(
            Term::iri("node1"),
            Term::iri("own"),
            Term::iri("point0")
        )));
        assert!(db.stats().index_valid, "a delete keeps the index valid");
        assert_eq!(db.vs_shrink_debt(), 1);

        let rs = db.select("SELECT ?o WHERE { <node1> <own> ?o }").unwrap();
        let got: Vec<_> = rs.rows.iter().map(|r| r[0].clone().unwrap()).collect();
        assert_eq!(got, vec!["<point1>".to_string()], "point0 was deleted");

        // Rebuilding clears the debt and gives the same answer.
        db.rebuild_index();
        assert!(db.stats().index_valid);
        assert_eq!(db.vs_shrink_debt(), 0);
        let rs = db.select("SELECT ?o WHERE { <node1> <own> ?o }").unwrap();
        assert_eq!(rs.row_count(), 1);
    }

    /// Deletes (and a rollback's removal of inserts) leave the VS-tree valid
    /// as a sound superset, and a rollback that re-adds deleted triples
    /// re-signs their entities: after every step, each entity whose current
    /// signature contains a query is among the tree's candidates, and the
    /// answers equal those of a fresh build.
    #[test]
    fn vstree_stays_a_sound_superset_under_deletes_and_rollbacks() {
        let mut seed = 0x5EED_u64;
        let mut next = move |n: u32| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((seed >> 33) % u64::from(n)) as u32
        };
        let triple = |s: u32, p: u32, o: u32| {
            Triple::new(
                Term::iri(format!("http://ex/e{s}")),
                Term::iri(format!("http://ex/p{p}")),
                Term::iri(format!("http://ex/e{o}")),
            )
        };
        let mut text = String::new();
        for _ in 0..1500 {
            let (s, p, o) = (next(300), next(6), next(300));
            text.push_str(&format!(
                "<http://ex/e{s}> <http://ex/p{p}> <http://ex/e{o}> .\n"
            ));
        }
        let mut db = Database::build_from_str("vs", &text).unwrap();
        let check = |db: &mut Database, step: usize| {
            assert!(db.stats().index_valid, "step {step}: the index stays valid");
            let truth = vstree_entries(&db.store);
            assert_eq!(
                truth,
                backend_vstree_entries(&db.store),
                "step {step}: fast path"
            );
            for &(e, sig) in truth.iter().step_by(7) {
                // A query made of each real edge of `e` must find `e`.
                for &(p, o) in &db.store.po_by_s(e) {
                    let mut q = Signature::new();
                    q.encode_query_edge(Some(p), Some(o), EdgeDir::Out);
                    assert!(sig.contains(&q));
                    let cands = db.vstree.candidates(&q).unwrap();
                    assert!(cands.contains(&e), "step {step}: entity {e} pruned");
                    assert!(db.vstree.probe(e, &q), "step {step}: probe misses {e}");
                }
            }
            let q = "SELECT ?s ?o WHERE { ?s <http://ex/p1> ?o . ?o <http://ex/p2> ?x }";
            let mut a = db.select(q).unwrap().rows;
            let saved = db.index_valid;
            db.index_valid = false;
            let mut b = db.select(q).unwrap().rows;
            db.index_valid = saved;
            a.sort();
            b.sort();
            assert_eq!(a, b, "step {step}: the tree changed an answer");
        };
        for step in 0..60 {
            match step % 4 {
                // Plain deletes and inserts.
                0 | 1 => {
                    for _ in 0..40 {
                        db.remove_triple(&triple(next(300), next(6), next(300)));
                    }
                    for (s, p, o) in db
                        .store
                        .iter_all()
                        .into_iter()
                        .take(30)
                        .map(|t| (t.sub, t.pred, t.obj))
                    {
                        db.remove_default_id(IdTriple::new(s, p, o));
                    }
                    for _ in 0..20 {
                        db.insert_triple(&triple(next(300), next(6), next(300)));
                    }
                }
                // A transaction deleting edges of an entity, then growing it
                // (its entry is re-signed without the deleted edges), rolled
                // back: the deleted edges come back and must be re-signed.
                _ => {
                    db.begin().unwrap();
                    let victims: Vec<IdTriple> = db
                        .store
                        .iter_all()
                        .into_iter()
                        .step_by(17)
                        .take(25)
                        .collect();
                    for t in &victims {
                        assert!(db.remove_default_id(*t));
                    }
                    for t in &victims {
                        let subject = db.dict.id_to_string(t.sub).unwrap().to_owned();
                        let grown = Triple::new(
                            Term::from_dict_key(&subject).unwrap(),
                            Term::iri(format!("http://ex/p{}", next(6))),
                            Term::iri(format!("http://ex/e{}", next(300))),
                        );
                        db.insert_triple(&grown);
                    }
                    db.rollback().unwrap();
                }
            }
            check(&mut db, step);
        }
        assert!(db.vs_shrink_debt() > 0);
    }

    /// Phase 6 R1 review: an insert into a valid index re-signed its subject
    /// and object from every edge they have, so after a delete (which keeps
    /// the index valid) each insert typing an entity into a large class read
    /// the whole class, and a rollback re-adding a deleted class did so once
    /// per triple — quadratic. An added edge only adds bits: it is ORed into
    /// the entries in O(height), and only a new entity is signed from the
    /// store. The answers stay those of an index-free evaluation.
    #[test]
    fn incremental_vstree_maintenance_does_not_rescan_hubs() {
        let n = 3000;
        let mut text = String::new();
        for i in 0..n {
            text.push_str(&format!(
                "<http://ex/e{i}> <http://ex/type> <http://ex/Class> .\n"
            ));
        }
        text.push_str("<http://ex/e0> <http://ex/knows> <http://ex/e1> .\n");
        let mut db = Database::build_from_str("hub", &text).unwrap();
        let resigned = || RESIGNED_EDGES.with(|c| c.replace(0));
        let agree = |db: &mut Database, q: &str| {
            let mut a = db.select(q).unwrap().rows;
            let saved = db.index_valid;
            db.index_valid = false;
            let mut b = db.select(q).unwrap().rows;
            db.index_valid = saved;
            a.sort();
            b.sort();
            assert_eq!(a, b, "the tree changed the answer of {q}");
            a.len()
        };
        resigned();
        // A delete keeps the index valid; then 200 typings into the class.
        db.query("DELETE DATA { <http://ex/e5> <http://ex/type> <http://ex/Class> }")
            .unwrap();
        let mut ins = String::from("INSERT DATA {\n");
        for i in 0..200 {
            ins.push_str(&format!(
                "<http://ex/new{i}> <http://ex/type> <http://ex/Class> .\n"
            ));
        }
        ins.push('}');
        db.query(&ins).unwrap();
        assert!(db.stats().index_valid);
        let work = resigned();
        assert!(
            work <= 2 * 200,
            "inserts re-read {work} edges (the class has {n})"
        );
        let q = "SELECT ?s WHERE { ?s <http://ex/type> <http://ex/Class> }";
        assert_eq!(agree(&mut db, q), n - 1 + 200);
        // Delete the whole class in a transaction and roll it back.
        db.begin().unwrap();
        db.query("DELETE WHERE { ?s <http://ex/type> <http://ex/Class> }")
            .unwrap();
        assert_eq!(agree(&mut db, q), 0);
        db.rollback().unwrap();
        assert!(db.stats().index_valid);
        let work = resigned();
        assert!(work <= 2 * n as u64, "the rollback re-read {work} edges");
        assert_eq!(agree(&mut db, q), n - 1 + 200);
        // A retyping (DELETE … INSERT … WHERE) moves the class wholesale.
        db.query(
            "DELETE { ?s <http://ex/type> <http://ex/Class> } \
             INSERT { ?s <http://ex/type> <http://ex/Other> } \
             WHERE { ?s <http://ex/type> <http://ex/Class> }",
        )
        .unwrap();
        let work = resigned();
        assert!(work <= 2 * n as u64, "the retyping re-read {work} edges");
        let other = "SELECT ?s WHERE { ?s <http://ex/type> <http://ex/Other> }";
        assert_eq!(agree(&mut db, other), n - 1 + 200);
        assert_eq!(
            agree(
                &mut db,
                "SELECT ?o WHERE { <http://ex/e0> <http://ex/knows> ?o }"
            ),
            1
        );
    }

    /// The disk build signs entities from one streaming SPO+OSP pass; the
    /// entries (and so the `vstree.kv` written from them) equal the generic
    /// per-entity signing over the same store — with literal objects, ids
    /// that are both subject and object, entities that are only objects,
    /// and deleted triples.
    #[test]
    fn disk_vstree_entries_equal_the_generic_signing() {
        let dir = temp_dir("disk_vstree_entries");
        fs::create_dir_all(&dir).unwrap();
        let mut seed = 0xD15C_u64;
        let mut next = move |n: u32| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((seed >> 33) % u64::from(n)) as u32
        };
        let mut lines = Vec::new();
        for _ in 0..1200 {
            let (s, p, o) = (next(200), next(5), next(320));
            let line = if next(4) == 0 {
                (
                    format!("<http://ex/e{s}>"),
                    format!("<http://ex/p{p}>"),
                    format!("\"v{o}\""),
                    true,
                )
            } else {
                (
                    format!("<http://ex/e{s}>"),
                    format!("<http://ex/p{p}>"),
                    format!("<http://ex/e{o}>"),
                    false,
                )
            };
            lines.push(line);
        }
        let text: String = lines
            .iter()
            .map(|(s, p, o, _)| format!("{s} {p} {o} .\n"))
            .collect();
        let mut ds = crate::kvstore::DiskStore::build_str(dir.join(KV_FILE), 64, &text).unwrap();
        for (s, p, o, lit) in lines.iter().step_by(4) {
            ds.delete_keys(s, p, o, *lit).unwrap();
        }
        let truth = vstree_entries(&ds);
        assert!(truth.len() > 200, "subjects plus object-only entities");
        assert_eq!(disk_vstree_entries(&ds).unwrap(), truth);
        drop(ds);
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- redo log --------------------------------------------------------

    #[test]
    fn redo_log_replays_committed_mutations() {
        let dir = temp_dir("redo_replay");
        let mut db = Database::build_from_str("rd", SMALL).unwrap();
        db.enable_redo_log_in(&dir).unwrap();
        assert!(db.redo_log_enabled());

        // A mix of auto-commit mutations.
        db.query("INSERT DATA { <root> <contain> <nodeX> }")
            .unwrap();
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
        db.query("INSERT DATA { <root> <contain> <n_pre> }")
            .unwrap();

        // Full backup: base snapshot + checkpoint at the current redo-log length.
        db.backup(&base_dir).unwrap();
        let base_ck = db.backup_checkpoint();
        assert!(base_ck > 0, "checkpoint records the pre-backup update");

        // Updates AFTER the full backup ⇒ first increment (insert + delete).
        db.query("INSERT DATA { <root> <contain> <n1> . <n1> <own> <p1> }")
            .unwrap();
        db.query("DELETE DATA { <root> <contain> <node0> }")
            .unwrap();
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
        assert!(
            base_only.triple_num() < live_count,
            "base predates the increments"
        );
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
        db.query("INSERT DATA { <root> <contain> <n_pre> }")
            .unwrap();
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
        db.insert_triple(&Triple::new(
            Term::iri("root"),
            Term::iri("contain"),
            Term::iri("api_node"),
        ));
        // (b) a transaction that is rolled back — must leave no trace
        db.begin().unwrap();
        db.query("INSERT DATA { <root> <contain> <ghost> }")
            .unwrap();
        db.rollback().unwrap();

        db.backup_incremental(&incr_dir).unwrap();
        let mut restored = Database::restore_incremental(&base_dir, &[&incr_dir]).unwrap();

        assert_eq!(
            restored
                .select("SELECT ?s WHERE { ?s <contain> <api_node> }")
                .unwrap()
                .row_count(),
            1,
            "direct-API insert must be captured by the redo-based increment"
        );
        assert_eq!(
            restored
                .select("SELECT ?s WHERE { ?s <contain> <ghost> }")
                .unwrap()
                .row_count(),
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
        db.query("INSERT DATA { <root> <contain> <nodeZ> }")
            .unwrap();
        assert_eq!(db.query_cache_len(), 0, "any write clears the cache");
    }

    // ---- id freelist reclamation -----------------------------------------

    #[test]
    fn reclaim_unused_reuses_deleted_term_ids() {
        let mut db = Database::build_from_str("rc", SMALL).unwrap();
        // The explicit pass: with immediate reclamation off (R4) nothing is
        // freed by the delete itself.
        db.set_id_reclaim(IdReclaim::Off);
        let before_entities = db.entity_num();
        let node0_id = db.dict().entity_id(&Term::iri("node0").dict_key()).unwrap();

        // Delete the only triple using <node0>; its id is not auto-freed.
        db.query("DELETE DATA { <root> <contain> <node0> }")
            .unwrap();
        assert!(db
            .dict()
            .entity_id(&Term::iri("node0").dict_key())
            .is_some());

        let freed = db.reclaim_unused().unwrap();
        assert_eq!(freed, 1, "only <node0> became unreferenced");
        assert_eq!(db.entity_num(), before_entities - 1);
        assert!(
            db.dict()
                .entity_id(&Term::iri("node0").dict_key())
                .is_none(),
            "node0 was reclaimed"
        );

        // A brand-new term reuses node0's freed id.
        db.query("INSERT DATA { <root> <contain> <fresh> }")
            .unwrap();
        assert_eq!(
            db.dict().entity_id(&Term::iri("fresh").dict_key()),
            Some(node0_id),
            "the freed id is reused"
        );
    }

    #[test]
    fn a_delete_frees_its_unreferenced_ids_immediately() {
        let mut db = Database::build_from_str("rci", SMALL).unwrap();
        assert_eq!(db.id_reclaim(), IdReclaim::Immediate);
        let before_entities = db.entity_num();
        let node0_id = db.dict().entity_id(&Term::iri("node0").dict_key()).unwrap();
        db.query("DELETE DATA { <root> <contain> <node0> }")
            .unwrap();
        assert_eq!(
            db.dict().entity_id(&Term::iri("node0").dict_key()),
            None,
            "freed by the delete"
        );
        assert_eq!(db.entity_num(), before_entities - 1);
        // <root> and <contain> are still used: kept.
        assert!(db.dict().entity_id(&Term::iri("root").dict_key()).is_some());
        assert_eq!(db.reclaim_unused().unwrap(), 0, "nothing left to free");
        db.query("INSERT DATA { <root> <contain> <fresh> }")
            .unwrap();
        assert_eq!(
            db.dict().entity_id(&Term::iri("fresh").dict_key()),
            Some(node0_id)
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
            content.push_str(&format!(
                "<http://ex/s{i}> <http://ex/p> <http://ex/o{i}> .\n"
            ));
        }
        content.push_str("<http://ex/special> <http://ex/marker> <http://ex/target> .\n");
        let rdf = std::env::temp_dir().join("gstore_ut_disk_vstree.nt");
        fs::write(&rdf, &content).unwrap();

        Database::build_disk(&dir, &[&rdf]).unwrap();
        assert!(Database::is_disk(&dir));
        // The dedicated on-disk VS-tree node file was produced.
        assert!(
            dir.join(VSTREE_KV_FILE).is_file(),
            "build_disk must write the VS-tree node file"
        );

        let mut db = Database::load_disk(&dir).unwrap();
        assert!(
            db.vstree.is_disk_backed(),
            "loaded disk db must use the on-disk VS-tree"
        );
        assert!(
            db.stats().index_valid,
            "the disk-backed index is valid for filtering"
        );

        // In-memory baseline over the same data (full VS-tree rebuild).
        let mut base = Database::build_from_str("base", &content).unwrap();
        let queries = [
            "SELECT ?s WHERE { ?s <http://ex/marker> <http://ex/target> }",
            "SELECT ?o WHERE { <http://ex/s5> <http://ex/p> ?o }",
            "SELECT ?s ?o WHERE { ?s <http://ex/p> ?o }",
            "SELECT ?s WHERE { ?s <http://ex/p> <http://ex/o42> }",
        ];
        let mut expected = Vec::new();
        {
            // The tree's own walk (the `Tree` policy pages its nodes; `Auto`,
            // the default, computes an out-of-core tree's sets from the store).
            let _tree = crate::query::scoped_vs_policy(crate::query::VsPolicy::Tree);
            for q in queries {
                let mut a = db.select(q).unwrap().rows;
                let mut b = base.select(q).unwrap().rows;
                a.sort();
                b.sort();
                assert_eq!(
                    a, b,
                    "disk-vstree query {q:?} diverged from in-memory baseline"
                );
                expected.push(b);
            }

            // The distinctive marker query returns exactly the special subject —
            // and the candidate filter that produced it ran against on-disk nodes.
            let rs = db
                .select("SELECT ?s WHERE { ?s <http://ex/marker> <http://ex/target> }")
                .unwrap();
            assert_eq!(rs.row_count(), 1);
            assert_eq!(rs.rows[0][0].as_deref(), Some("<http://ex/special>"));
            // Running the query touched only some VS-tree node pages, not the
            // whole tree — proving it need not be fully resident.
            assert!(
                db.vstree.disk_pages_read() > 0,
                "the disk VS-tree was actually traversed"
            );
        }
        // Under the default policy the answers are the same, and no page of
        // the tree is read.
        let pages = db.vstree.disk_pages_read();
        for (q, b) in queries.into_iter().zip(&expected) {
            let mut a = db.select(q).unwrap().rows;
            a.sort();
            assert_eq!(&a, b, "query {q:?} under the default policy");
        }
        assert_eq!(db.vstree.disk_pages_read(), pages);

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_file(&rdf);
    }

    /// Mutating a disk-loaded database invalidates the out-of-core index (it
    /// can't be updated in place), and queries stay correct afterwards.
    /// (A rebuild gives a Materialize-mode database an in-memory tree and a
    /// Stream-mode one a rewritten `vstree.kv`, fresh in the store's header,
    /// so the next open uses it.)
    #[test]
    fn disk_database_mutation_invalidates_then_stays_correct() {
        for mode in [DiskQueryMode::Materialize, DiskQueryMode::Stream] {
            let dir = temp_dir(&format!("disk_vstree_mut_{mode}"));
            let content = "<a> <p> <b> .\n<b> <p> <c> .\n";
            let rdf = std::env::temp_dir().join("gstore_ut_disk_vstree_mut.nt");
            fs::write(&rdf, content).unwrap();
            Database::build_disk(&dir, &[&rdf]).unwrap();
            let options = OpenOptions::new().with_disk_query_mode(mode);
            let mut db = Database::load_disk_with(&dir, &options).unwrap();
            assert_eq!(db.disk_query_mode(), Some(mode));
            assert!(db.vstree.is_disk_backed() && db.stats().index_valid);

            // An insert can't update the on-disk tree in place ⇒ index invalidates.
            assert!(db.insert_triple(&Triple::new(Term::iri("c"), Term::iri("p"), Term::iri("d"))));
            assert!(
                !db.stats().index_valid,
                "mutation invalidates the disk-backed index"
            );

            // Query still correct (evaluates without the stale tree).
            let mut got = db.select("SELECT ?o WHERE { <c> <p> ?o }").unwrap().rows;
            got.sort();
            assert_eq!(got, vec![vec![Some("<d>".to_string())]]);

            db.rebuild_index();
            assert!(db.stats().index_valid);
            match mode {
                // A rebuild restores a valid (now in-memory) index.
                DiskQueryMode::Materialize => assert!(!db.vstree.is_disk_backed()),
                // A rebuild rewrites vstree.kv; a reopen trusts it again.
                DiskQueryMode::Stream => {
                    assert!(db.vstree.is_disk_backed());
                    let rows = db.select("SELECT ?s WHERE { ?s <p> <d> }").unwrap().rows;
                    assert_eq!(rows, vec![vec![Some("<c>".to_string())]]);
                    drop(db);
                    let db = Database::load_disk_with(&dir, &options).unwrap();
                    assert!(db.vstree.is_disk_backed() && db.stats().index_valid);
                }
            }
            let _ = fs::remove_dir_all(&dir);
            let _ = fs::remove_file(&rdf);
        }
    }

    /// An **in-memory** database can opt into an out-of-core (streaming) VS-tree
    /// and answer queries identically to the default in-memory tree, while the
    /// filter streams nodes from disk (proving the whole tree need not be
    /// resident). This is the in-memory query path using the out-of-core seam.
    #[test]
    fn streaming_vstree_matches_in_memory_path() {
        let mut content = String::new();
        for i in 0..400u32 {
            content.push_str(&format!(
                "<http://ex/s{i}> <http://ex/p> <http://ex/o{i}> .\n"
            ));
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
        assert!(
            streamed.vstree.is_disk_backed(),
            "opted into out-of-core VS-tree"
        );
        assert!(
            streamed.stats().index_valid,
            "streaming index is valid for filtering"
        );
        // The store stayed in memory; only the filter went out-of-core.
        assert!(matches!(streamed.store, Backend::Memory(_)));

        let queries = [
            "SELECT ?s WHERE { ?s <http://ex/marker> <http://ex/target> }",
            "SELECT ?o WHERE { <http://ex/s5> <http://ex/p> ?o }",
            "SELECT ?s ?o WHERE { ?s <http://ex/p> ?o }",
            "SELECT ?s WHERE { ?s <http://ex/p> <http://ex/o42> }",
        ];
        let mut expected = Vec::new();
        {
            // The tree's own walk (see `disk_database_filters_via_on_disk_vstree`).
            let _tree = crate::query::scoped_vs_policy(crate::query::VsPolicy::Tree);
            for q in queries {
                let mut a = streamed.select(q).unwrap().rows;
                let mut b = base.select(q).unwrap().rows;
                a.sort();
                b.sort();
                assert_eq!(a, b, "streaming-vstree query {q:?} diverged from in-memory");
                expected.push(b);
            }
            // The candidate filter actually paged VS-tree nodes off disk.
            assert!(
                streamed.vstree.disk_pages_read() > 0,
                "streaming VS-tree was traversed on disk"
            );
        }
        // The default policy: the same answers, no page of the tree read.
        let pages = streamed.vstree.disk_pages_read();
        for (q, b) in queries.into_iter().zip(&expected) {
            let mut a = streamed.select(q).unwrap().rows;
            a.sort();
            assert_eq!(&a, b, "query {q:?} under the default policy");
        }
        assert_eq!(streamed.vstree.disk_pages_read(), pages);
        let _ = fs::remove_dir_all(&dir);
    }

    // ---- build progress reporting ----------------------------------------

    #[test]
    fn build_reports_progress_stages() {
        let stages = std::cell::RefCell::new(Vec::new());
        let db =
            Database::build_from_str_with_progress("p", SMALL, |s| stages.borrow_mut().push(s))
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
