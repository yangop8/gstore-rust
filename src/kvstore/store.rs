//! A disk-backed triple store + dictionary built from B+ trees.
//!
//! Mirrors gStore's `KVstore`: the dictionary trees (`entity2id`/`literal2id`/
//! `predicate2id` and their inverses) plus the triple value indexes. Triples are
//! held in three ordered B+ trees — SPO, POS, OSP — with 12-byte composite keys
//! (big-endian `subject|predicate|object` in each ordering). Prefix range scans
//! over these orderings answer every access pattern, exactly as the in-memory
//! [`crate::store::TripleStore`] does, but entirely from disk through the page
//! cache. Data persists and reopens.

use std::cell::RefCell;
use std::path::Path;

use crate::dict::Dictionary;
use crate::error::{GStoreError, Result};
use crate::model::id::{is_literal_id, EntityLiteralId, PredId, LITERAL_FIRST_ID};
use crate::model::{IdTriple, Term, Triple};
use crate::parser::{sparql, turtle};
use crate::query::{Evaluator, QueryResult};
use crate::store::{TripleSource, TripleStore};

use super::bptree::{be32, de32, BTree};
use super::pager::Pager;

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

/// A disk-backed gStore database (dictionary + six-way triple index).
pub struct DiskStore {
    pager: RefCell<Pager>,
    spo: BTree,
    pos: BTree,
    osp: BTree,
    entity2id: BTree,
    literal2id: BTree,
    predicate2id: BTree,
    id2entity: BTree,
    id2literal: BTree,
    id2predicate: BTree,
    triple_count: u64,
    entity_count: u32,
    literal_count: u32,
    pred_count: u32,
}

impl DiskStore {
    /// Open (or create) a disk store at `path` with a `cache_pages`-page cache.
    pub fn open<P: AsRef<Path>>(path: P, cache_pages: usize) -> Result<DiskStore> {
        let pager = Pager::open(path, cache_pages)?;
        let triple_count = pager.root(ROOT_TRIPLE_COUNT);
        let entity_count = pager.root(ROOT_ENTITY_COUNT) as u32;
        let literal_count = pager.root(ROOT_LITERAL_COUNT) as u32;
        let pred_count = pager.root(ROOT_PRED_COUNT) as u32;
        Ok(DiskStore {
            pager: RefCell::new(pager),
            spo: BTree::new(SPO),
            pos: BTree::new(POS),
            osp: BTree::new(OSP),
            entity2id: BTree::new(ENTITY2ID),
            literal2id: BTree::new(LITERAL2ID),
            predicate2id: BTree::new(PREDICATE2ID),
            id2entity: BTree::new(ID2ENTITY),
            id2literal: BTree::new(ID2LITERAL),
            id2predicate: BTree::new(ID2PREDICATE),
            triple_count,
            entity_count,
            literal_count,
            pred_count,
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
        let pager = self.pager.get_mut();
        if let Some(v) = self.entity2id.get(pager, key.as_bytes())? {
            return Ok(de32(&v));
        }
        let id = self.entity_count;
        self.entity2id.insert(pager, key.as_bytes(), &be32(id))?;
        self.id2entity.insert(pager, &be32(id), key.as_bytes())?;
        self.entity_count += 1;
        Ok(id)
    }

    fn intern_literal(&mut self, key: &str) -> Result<EntityLiteralId> {
        let pager = self.pager.get_mut();
        if let Some(v) = self.literal2id.get(pager, key.as_bytes())? {
            return Ok(de32(&v));
        }
        let id = LITERAL_FIRST_ID
            .checked_add(self.literal_count)
            .ok_or_else(|| GStoreError::Database("literal id space exhausted".to_string()))?;
        self.literal2id.insert(pager, key.as_bytes(), &be32(id))?;
        self.id2literal.insert(pager, &be32(id), key.as_bytes())?;
        self.literal_count += 1;
        Ok(id)
    }

    fn intern_predicate(&mut self, key: &str) -> Result<PredId> {
        let pager = self.pager.get_mut();
        if let Some(v) = self.predicate2id.get(pager, key.as_bytes())? {
            return Ok(de32(&v));
        }
        let id = self.pred_count;
        self.predicate2id.insert(pager, key.as_bytes(), &be32(id))?;
        self.id2predicate.insert(pager, &be32(id), key.as_bytes())?;
        self.pred_count += 1;
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

    fn insert_ids(&mut self, t: IdTriple) -> Result<bool> {
        let pager = self.pager.get_mut();
        let spo = key3(t.sub, t.pred, t.obj);
        if self.spo.get(pager, &spo)?.is_some() {
            return Ok(false);
        }
        self.spo.insert(pager, &spo, b"")?;
        self.pos.insert(pager, &key3(t.pred, t.obj, t.sub), b"")?;
        self.osp.insert(pager, &key3(t.obj, t.sub, t.pred), b"")?;
        self.triple_count += 1;
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
            let pager = self.pager.get_mut();
            match self.entity2id.get(pager, t.subject.dict_key().as_bytes())? {
                Some(v) => de32(&v),
                None => return Ok(false),
            }
        };
        let p = {
            let pager = self.pager.get_mut();
            match self.predicate2id.get(pager, t.predicate.dict_key().as_bytes())? {
                Some(v) => de32(&v),
                None => return Ok(false),
            }
        };
        let o = {
            let pager = self.pager.get_mut();
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
        let pager = self.pager.get_mut();
        let spo = key3(t.sub, t.pred, t.obj);
        if self.spo.get(pager, &spo)?.is_none() {
            return Ok(false);
        }
        self.spo.delete(pager, &spo)?;
        self.pos.delete(pager, &key3(t.pred, t.obj, t.sub))?;
        self.osp.delete(pager, &key3(t.obj, t.sub, t.pred))?;
        self.triple_count -= 1;
        Ok(true)
    }

    /// Persist counters and flush all dirty pages.
    pub fn flush(&mut self) -> Result<()> {
        let pager = self.pager.get_mut();
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
        let mut pager = self.pager.borrow_mut();
        let tree = if t.is_literal() {
            &self.literal2id
        } else {
            &self.entity2id
        };
        Ok(tree.get(&mut pager, key.as_bytes())?.map(|v| de32(&v)))
    }

    pub fn predicate_id(&self, dict_key: &str) -> Result<Option<PredId>> {
        let mut pager = self.pager.borrow_mut();
        Ok(self
            .predicate2id
            .get(&mut pager, dict_key.as_bytes())?
            .map(|v| de32(&v)))
    }

    pub fn id_to_string(&self, id: EntityLiteralId) -> Result<Option<String>> {
        let mut pager = self.pager.borrow_mut();
        let tree = if is_literal_id(id) {
            &self.id2literal
        } else {
            &self.id2entity
        };
        Ok(tree
            .get(&mut pager, &be32(id))?
            .map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    pub fn predicate_to_string(&self, id: PredId) -> Result<Option<String>> {
        let mut pager = self.pager.borrow_mut();
        Ok(self
            .id2predicate
            .get(&mut pager, &be32(id))?
            .map(|b| String::from_utf8_lossy(&b).into_owned()))
    }

    // ---- access patterns (mirror TripleStore) ----------------------------

    pub fn exists(&self, s: EntityLiteralId, p: PredId, o: EntityLiteralId) -> Result<bool> {
        let mut pager = self.pager.borrow_mut();
        Ok(self.spo.get(&mut pager, &key3(s, p, o))?.is_some())
    }

    /// `s ? ?` → `(pred, obj)` pairs (sorted by (pred, obj)).
    pub fn po_by_s(&self, s: EntityLiteralId) -> Result<Vec<(PredId, EntityLiteralId)>> {
        let rows = self.scan(&self.spo, &be32(s))?;
        Ok(rows
            .iter()
            .map(|k| (de32(&k[4..8]), de32(&k[8..12])))
            .collect())
    }

    /// `s p ?` → objects.
    pub fn o_by_sp(&self, s: EntityLiteralId, p: PredId) -> Result<Vec<EntityLiteralId>> {
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
        let mut pager = self.pager.borrow_mut();
        Ok(tree
            .scan_prefix(&mut pager, prefix)?
            .into_iter()
            .map(|(k, _)| k)
            .collect())
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

    /// Answer a SPARQL read query (SELECT/ASK/CONSTRUCT/DESCRIBE) by *streaming*
    /// matches directly from the on-disk indexes — only the dictionary is held in
    /// memory; the triple indexes are read on demand through the page cache. This
    /// lets a database larger than RAM be queried without materializing it (the
    /// [`to_memory`](Self::to_memory) path does materialize). Updates and the
    /// VS-tree filter are not available on this read-only streaming path.
    pub fn query(&self, sparql: &str) -> Result<QueryResult> {
        let dict = self.dictionary()?;
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
}
