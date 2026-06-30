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
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::dict::Dictionary;
use crate::error::{GStoreError, Result};
use crate::kvstore::DiskStore;
use crate::model::id::is_entity_id;
use crate::model::{IdTriple, Term, Triple};
use crate::parser::sparql::ast::{GraphTarget, GroundTriple, Query, UpdateOp, RDF_TYPE};
use crate::parser::{sparql, turtle};
use crate::query::{Evaluator, QueryResult};
use crate::signature::{EdgeDir, Signature, VsTree};
use crate::store::TripleStore;

const RDFS_SUBCLASS: &str = "http://www.w3.org/2000/01/rdf-schema#subClassOf";
const RDFS_SUBPROP: &str = "http://www.w3.org/2000/01/rdf-schema#subPropertyOf";
const RDFS_DOMAIN: &str = "http://www.w3.org/2000/01/rdf-schema#domain";
const RDFS_RANGE: &str = "http://www.w3.org/2000/01/rdf-schema#range";

const DICT_FILE: &str = "dict.bin";
const STORE_FILE: &str = "store.bin";
const META_FILE: &str = "meta.bin";
const VSTREE_FILE: &str = "vstree.bin";
const NAMED_FILE: &str = "named.bin";
/// The on-disk B+ tree KVstore file inside a database directory.
const KV_FILE: &str = "kvstore.kv";
/// Page-cache size for the disk store (4096 × 4 KiB = 16 MiB).
const DISK_CACHE_PAGES: usize = 4096;

/// Build a VS-tree over every entity, signing each by its in/out edges.
fn build_vstree(store: &TripleStore) -> VsTree {
    // Entities = everything that is a subject, plus objects that are entities
    // (literal objects are not indexed by the VS-tree).
    let mut ids: Vec<u32> = store.subject_keys().collect();
    ids.extend(store.object_keys().filter(|&o| is_entity_id(o)));
    ids.sort_unstable();
    ids.dedup();

    let entries = ids
        .into_iter()
        .map(|e| {
            let mut sig = Signature::new();
            for &(p, o) in store.po_by_s(e) {
                sig.encode_edge(p, o, EdgeDir::Out);
            }
            for &(p, s) in store.ps_by_o(e) {
                sig.encode_edge(p, s, EdgeDir::In);
            }
            (e, sig)
        })
        .collect();
    VsTree::build(entries)
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

/// An RDF database: a dictionary, the six-way triple index, and a VS-tree.
#[derive(Debug)]
pub struct Database {
    name: String,
    dict: Dictionary,
    store: TripleStore,
    /// Named graphs: graph-IRI entity id → its triple store. The default graph
    /// is `store`; `GRAPH` patterns and quad updates target this map.
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
    /// Cache of read-query results keyed by the SPARQL string (gStore
    /// `QueryCache`), cleared on any store mutation. Interior mutability so a
    /// `&self`/`&mut self` query path can read and populate it.
    query_cache: RefCell<HashMap<String, QueryResult>>,
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
            store: TripleStore::new(),
            named: BTreeMap::new(),
            vstree: VsTree::new(),
            index_valid: true, // empty store ⇔ empty tree, trivially consistent
            txn: None,
            query_cache: RefCell::new(HashMap::new()),
        }
    }

    /// Rebuild the VS-tree from the current store and mark the index valid.
    pub fn rebuild_index(&mut self) {
        self.vstree = build_vstree(&self.store);
        self.index_valid = true;
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
        let mut db = Database::new(name);
        let mut id_triples: Vec<IdTriple> = Vec::new();
        for t in turtle::parse_str(content)? {
            id_triples.push(db.encode_triple(&t));
        }
        db.store.bulk_load(id_triples);
        db.rebuild_index();
        Ok(db)
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
    pub fn store(&self) -> &TripleStore {
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
            for &(s, o) in self.store.so_by_p(sco) {
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
            .filter_map(|p| self.dict.predicate_to_string(p).map(str::to_owned))
            .collect();
        for iri in [RDFS_SUBPROP, RDFS_DOMAIN, RDFS_RANGE] {
            if let Some(p) = pid(iri) {
                for &(s, _o) in self.store.so_by_p(p) {
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
            self.index_valid = false; // VS-tree now stale
            self.query_cache.get_mut().clear();
            if let Some(log) = self.txn.as_mut() {
                log.push(UndoOp::Del(None, id)); // rollback removes what we added
            }
        }
        changed
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
            self.index_valid = false;
            self.query_cache.get_mut().clear();
            if let Some(log) = self.txn.as_mut() {
                log.push(UndoOp::Add(None, id)); // rollback re-adds what we removed
            }
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

    /// Commit the active transaction, discarding its undo log.
    pub fn commit(&mut self) -> Result<()> {
        if self.txn.take().is_none() {
            return Err(GStoreError::Database("no active transaction".into()));
        }
        Ok(())
    }

    /// Roll back the active transaction, undoing every triple mutation made
    /// since [`begin`](Self::begin) in reverse order.
    pub fn rollback(&mut self) -> Result<()> {
        let Some(log) = self.txn.take() else {
            return Err(GStoreError::Database("no active transaction".into()));
        };
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
                if let Some(cached) = self.query_cache.borrow().get(sparql) {
                    return Ok(cached.clone());
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
                let result = eval.evaluate(&q)?;
                self.query_cache
                    .borrow_mut()
                    .insert(sparql.to_string(), result.clone());
                Ok(result)
            }
            Query::Update(ops) => {
                let mut changed = 0;
                for op in ops {
                    changed += self.exec_update_op(op)?;
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
            // Record undo entries before discarding the store, so CLEAR inside a
            // transaction can be rolled back.
            if let Some(log) = self.txn.as_mut() {
                for t in self.store.iter_all() {
                    log.push(UndoOp::Add(None, t));
                }
            }
            self.store = TripleStore::new();
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
    pub fn save<P: AsRef<Path>>(&self, dir: P) -> Result<()> {
        let dir = dir.as_ref();
        fs::create_dir_all(dir)?;
        write_bincode(&dir.join(DICT_FILE), &self.dict)?;
        write_bincode(&dir.join(STORE_FILE), &self.store)?;
        write_bincode(&dir.join(NAMED_FILE), &self.named)?;
        let meta = Meta {
            name: self.name.clone(),
            triple_num: self.store.triple_count(),
            entity_num: self.dict.entity_num() as u64,
            literal_num: self.dict.literal_num() as u64,
            predicate_num: self.dict.predicate_num() as u64,
        };
        write_bincode(&dir.join(META_FILE), &meta)?;
        // Persist a fresh VS-tree (rebuild if the in-memory one is stale).
        if self.index_valid {
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
        let kv = dir.join(KV_FILE);
        let _ = fs::remove_file(&kv); // fresh build
        DiskStore::build_files(&kv, DISK_CACHE_PAGES, files)?;
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
        let mut db = Database {
            name,
            dict,
            store,
            named: BTreeMap::new(),
            vstree: VsTree::new(),
            index_valid: false,
            txn: None,
            query_cache: RefCell::new(HashMap::new()),
        };
        db.rebuild_index();
        Ok(db)
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
            store,
            named,
            vstree,
            index_valid: true,
            txn: None,
            query_cache: RefCell::new(HashMap::new()),
        })
    }
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
}
