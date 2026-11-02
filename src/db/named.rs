//! The named graphs of a [`Database`](super::Database), with the reference
//! counts id reclamation needs.
//!
//! [`NamedGraphs`] is the map *graph-IRI entity id → its triples* the
//! database always kept (it dereferences to that `BTreeMap`, and serializes
//! exactly as it, so `named.bin` is unchanged), plus two counters per id:
//!
//! * `node_refs[x]` — how many quads use entity/literal `x` as subject or as
//!   object (a quad whose subject is its object counts twice), plus one if
//!   `x` keys a graph;
//! * `pred_refs[p]` — how many quads use predicate `p`.
//!
//! A term is referenced by the named graphs exactly when its counter is
//! non-zero. These counts are authoritative for every layout: the on-disk
//! `GSPO` index is keyed graph-first, so it cannot answer "is `x` used
//! anywhere?" without a scan. Every mutation goes through the methods here,
//! which keep the counts and drop a graph the moment it holds no quad (this
//! store keeps no empty named graph). [`verify_refs`](NamedGraphs::verify_refs)
//! recomputes them.

use std::collections::{BTreeMap, HashMap};
use std::ops::Deref;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::model::id::{EntityLiteralId, PredId};
use crate::model::IdTriple;
use crate::store::TripleStore;

/// The named graphs and their term reference counts (see the module docs).
#[derive(Clone, Default)]
pub struct NamedGraphs {
    graphs: BTreeMap<u32, TripleStore>,
    node_refs: HashMap<EntityLiteralId, u32>,
    pred_refs: HashMap<PredId, u32>,
}

impl std::fmt::Debug for NamedGraphs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NamedGraphs")
            .field("graphs", &self.graphs.len())
            .field("quads", &self.quad_count())
            .finish()
    }
}

impl Deref for NamedGraphs {
    type Target = BTreeMap<u32, TripleStore>;
    fn deref(&self) -> &BTreeMap<u32, TripleStore> {
        &self.graphs
    }
}

impl Serialize for NamedGraphs {
    /// Exactly the map's encoding (`named.bin` is unchanged).
    fn serialize<S: Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        self.graphs.serialize(ser)
    }
}

impl<'de> Deserialize<'de> for NamedGraphs {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<NamedGraphs, D::Error> {
        BTreeMap::<u32, TripleStore>::deserialize(de).map(NamedGraphs::from_map)
    }
}

impl From<BTreeMap<u32, TripleStore>> for NamedGraphs {
    fn from(map: BTreeMap<u32, TripleStore>) -> NamedGraphs {
        NamedGraphs::from_map(map)
    }
}

fn bump<K: std::hash::Hash + Eq>(m: &mut HashMap<K, u32>, k: K) {
    *m.entry(k).or_insert(0) += 1;
}

fn drop_ref<K: std::hash::Hash + Eq>(m: &mut HashMap<K, u32>, k: K) {
    if let std::collections::hash_map::Entry::Occupied(mut e) = m.entry(k) {
        if *e.get() <= 1 {
            e.remove();
        } else {
            *e.get_mut() -= 1;
        }
    }
}

impl NamedGraphs {
    /// No named graph.
    pub fn new() -> NamedGraphs {
        NamedGraphs::default()
    }

    /// The named graphs of `map` (empty graphs are dropped), counted.
    pub fn from_map(mut map: BTreeMap<u32, TripleStore>) -> NamedGraphs {
        map.retain(|_, s| s.triple_count() > 0);
        let mut out = NamedGraphs {
            graphs: map,
            ..NamedGraphs::default()
        };
        let (nodes, preds) = out.recount();
        out.node_refs = nodes;
        out.pred_refs = preds;
        out
    }

    /// The plain map (the counts are dropped).
    pub fn into_map(self) -> BTreeMap<u32, TripleStore> {
        self.graphs
    }

    /// Total number of quads.
    pub fn quad_count(&self) -> u64 {
        self.graphs.values().map(TripleStore::triple_count).sum()
    }

    fn count_quad(&mut self, t: IdTriple) {
        bump(&mut self.node_refs, t.sub);
        bump(&mut self.node_refs, t.obj);
        bump(&mut self.pred_refs, t.pred);
    }

    fn uncount_quad(&mut self, t: IdTriple) {
        drop_ref(&mut self.node_refs, t.sub);
        drop_ref(&mut self.node_refs, t.obj);
        drop_ref(&mut self.pred_refs, t.pred);
    }

    /// Add quad `(g, t)`; `true` if it was new (a new graph is created).
    pub fn insert(&mut self, g: u32, t: IdTriple) -> bool {
        let graph = self.graphs.entry(g).or_default();
        let fresh_graph = graph.triple_count() == 0;
        if !graph.insert(t) {
            if fresh_graph {
                self.graphs.remove(&g);
            }
            return false;
        }
        if fresh_graph {
            bump(&mut self.node_refs, g);
        }
        self.count_quad(t);
        true
    }

    /// Remove quad `(g, t)`; `true` if it existed. A graph left empty is
    /// dropped.
    pub fn remove(&mut self, g: u32, t: IdTriple) -> bool {
        let Some(graph) = self.graphs.get_mut(&g) else {
            return false;
        };
        if !graph.remove(t) {
            return false;
        }
        let emptied = graph.triple_count() == 0;
        self.uncount_quad(t);
        if emptied {
            self.graphs.remove(&g);
            drop_ref(&mut self.node_refs, g);
        }
        true
    }

    /// Remove graph `g` whole, returning its triples.
    pub fn remove_graph(&mut self, g: u32) -> Option<TripleStore> {
        let graph = self.graphs.remove(&g)?;
        for t in graph.iter_all() {
            self.uncount_quad(t);
        }
        drop_ref(&mut self.node_refs, g);
        Some(graph)
    }

    /// Remove every graph, returning them.
    pub fn take_all(&mut self) -> BTreeMap<u32, TripleStore> {
        self.node_refs.clear();
        self.pred_refs.clear();
        std::mem::take(&mut self.graphs)
    }

    /// Add `triples` to graph `g` (duplicates ignored).
    pub fn bulk_load(&mut self, g: u32, triples: impl IntoIterator<Item = IdTriple>) {
        if let Some(old) = self.remove_graph(g) {
            let mut all: Vec<IdTriple> = old.iter_all().collect();
            all.extend(triples);
            return self.load_new(g, all);
        }
        self.load_new(g, triples);
    }

    fn load_new(&mut self, g: u32, triples: impl IntoIterator<Item = IdTriple>) {
        let mut graph = TripleStore::new();
        crate::store::MutableStore::bulk_load(&mut graph, triples.into_iter().collect());
        if graph.triple_count() == 0 {
            return;
        }
        for t in graph.iter_all() {
            self.count_quad(t);
        }
        bump(&mut self.node_refs, g);
        self.graphs.insert(g, graph);
    }

    /// How often entity/literal `id` is used by the named graphs (see the
    /// module docs).
    pub fn node_refs(&self, id: EntityLiteralId) -> u32 {
        self.node_refs.get(&id).copied().unwrap_or(0)
    }

    /// How often predicate `p` is used by the named graphs.
    pub fn pred_refs(&self, p: PredId) -> u32 {
        self.pred_refs.get(&p).copied().unwrap_or(0)
    }

    /// The counts recomputed from the graphs.
    fn recount(&self) -> (HashMap<EntityLiteralId, u32>, HashMap<PredId, u32>) {
        let mut nodes = HashMap::new();
        let mut preds = HashMap::new();
        for (&g, graph) in &self.graphs {
            if graph.triple_count() == 0 {
                continue;
            }
            bump(&mut nodes, g);
            for t in graph.iter_all() {
                bump(&mut nodes, t.sub);
                bump(&mut nodes, t.obj);
                bump(&mut preds, t.pred);
            }
        }
        (nodes, preds)
    }

    /// Check the maintained counts against a recount (and that no graph is
    /// empty); `Err` names the first difference.
    pub fn verify_refs(&self) -> Result<(), String> {
        if let Some((g, _)) = self.graphs.iter().find(|(_, s)| s.triple_count() == 0) {
            return Err(format!("named graph {g} is empty but kept"));
        }
        let (nodes, preds) = self.recount();
        if nodes != self.node_refs {
            let bad = nodes
                .iter()
                .find(|(k, v)| self.node_refs.get(k) != Some(v))
                .map(|(k, _)| *k)
                .or_else(|| {
                    self.node_refs
                        .keys()
                        .find(|k| !nodes.contains_key(k))
                        .copied()
                });
            return Err(format!(
                "named-graph node reference counts differ from a recount (id {bad:?})"
            ));
        }
        if preds != self.pred_refs {
            return Err("named-graph predicate reference counts differ from a recount".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SplitMix64.
    struct Rng(u64);
    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            (z ^ (z >> 31)) % n
        }
    }

    #[test]
    fn refs_follow_random_operations() {
        let mut rng = Rng(7);
        let mut n = NamedGraphs::new();
        for step in 0..20_000 {
            let g = rng.below(6) as u32 + 100;
            let t = IdTriple::new(
                rng.below(20) as u32,
                rng.below(4) as u32,
                rng.below(20) as u32 + if rng.below(2) == 0 { 0 } else { 100 },
            );
            match rng.below(10) {
                0..=4 => {
                    n.insert(g, t);
                }
                5..=7 => {
                    n.remove(g, t);
                }
                8 => {
                    if rng.below(20) == 0 {
                        n.remove_graph(g);
                    } else {
                        n.bulk_load(g, [t, IdTriple::new(t.obj % 100, t.pred, t.sub)]);
                    }
                }
                _ => {
                    if rng.below(200) == 0 {
                        n.take_all();
                    }
                }
            }
            if step % 997 == 0 {
                n.verify_refs()
                    .unwrap_or_else(|e| panic!("step {step}: {e}"));
            }
        }
        n.verify_refs().unwrap();
        assert!(n.values().all(|s| s.triple_count() > 0));
    }

    #[test]
    fn graph_keys_and_quads_pin_terms() {
        let mut n = NamedGraphs::new();
        let t = IdTriple::new(1, 2, 3);
        assert!(n.insert(9, t));
        assert!(!n.insert(9, t));
        assert_eq!((n.node_refs(9), n.node_refs(1), n.node_refs(3)), (1, 1, 1));
        assert_eq!(n.pred_refs(2), 1);
        // A self-loop counts its node twice.
        n.insert(9, IdTriple::new(5, 2, 5));
        assert_eq!(n.node_refs(5), 2);
        assert!(n.remove(9, t));
        assert_eq!(n.node_refs(1), 0);
        assert!(n.remove(9, IdTriple::new(5, 2, 5)));
        assert!(n.is_empty(), "an emptied graph is dropped");
        assert_eq!((n.node_refs(9), n.pred_refs(2)), (0, 0));
    }

    #[test]
    fn serializes_exactly_as_the_map() {
        let mut n = NamedGraphs::new();
        n.insert(4, IdTriple::new(1, 2, 3));
        n.insert(7, IdTriple::new(3, 2, 1));
        let map: BTreeMap<u32, TripleStore> = n.clone().into_map();
        let bytes = bincode::serialize(&n).unwrap();
        assert_eq!(bytes, bincode::serialize(&map).unwrap());
        let back: NamedGraphs = bincode::deserialize(&bytes).unwrap();
        back.verify_refs().unwrap();
        assert_eq!(back.node_refs(4), 1);
        // An empty graph in an old file is dropped on load.
        let mut with_empty = map;
        with_empty.insert(11, TripleStore::new());
        let back: NamedGraphs =
            bincode::deserialize(&bincode::serialize(&with_empty).unwrap()).unwrap();
        assert!(!back.contains_key(&11));
        back.verify_refs().unwrap();
    }
}
