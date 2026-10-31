//! Per-predicate CSR graph for gStore's advanced graph functions.
//!
//! Mirrors upstream gStore's `CSR` (`src/Database/CSR.{h,cpp}`) exactly as
//! `GeneralEvaluation::loadCSR` builds it (`src/Query/GeneralEvaluation.cpp`
//! ~137-226) and `CSRQueryHandler` reads it (`src/Query/Algorithm/
//! CSRQueryHandler.cpp`):
//!
//! * **Entities only.** A triple `(s, p, o)` becomes an edge `s -p-> o` only
//!   when `o` is an entity (IRI / blank node); triples with a literal object are
//!   skipped (GE.cpp ~163-170, ~199-200). Subjects are always entities.
//! * **Per predicate.** Edges keep their predicate. Two triples `(s, p1, o)`
//!   and `(s, p2, o)` are two edges (one per predicate) — exactly like
//!   upstream's one-CSR-per-predicate layout; nothing is de-duplicated across
//!   predicates. (Within one predicate a triple is unique, so there are no
//!   parallel edges with the same label.)
//! * **Both directions.** Upstream keeps `csr[0]` (out-edges: subject ->
//!   objects) and `csr[1]` (in-edges: object -> subjects). Here [`Dir::Out`]
//!   and [`Dir::In`] address the same two adjacencies.
//! * **Vertex set.** `n` = the number of distinct entities that occur in at
//!   least one entity–entity edge (any predicate) — upstream's `csr[1].n`
//!   (GE.cpp ~209-216, `CSRQueryHandler::getVertNum`). `m` = the total number
//!   of entity–entity edges, counted per predicate (`csr[1].m`, GE.cpp ~218-221).
//!   Self-loops `(s, p, s)` are kept.
//! * **Neighbour order.** Within one predicate, out-neighbours are listed in
//!   ascending object id and in-neighbours in ascending subject id (upstream
//!   reads both lists from the sorted KV indexes). Node ids are assigned in
//!   ascending entity-id order (node `i` < node `j` ⇔ entity `i` < entity `j`),
//!   so "ascending node id" and "ascending entity id" are the same order and
//!   every upstream iteration order is preserved.
//!
//! # Node ids versus entity ids
//!
//! Upstream algorithms address vertices by their raw *entity id* and size their
//! scratch arrays by `getVertNum()`; whole-graph algorithms iterate
//! `vid = 0 .. getVertNum()` treating `vid` as an entity id. That coincides with
//! the real vertex set only when the entity ids `0..n` are exactly the vertices
//! (true for small, freshly loaded datasets such as the documentation's sample
//! graph); otherwise upstream reads out of bounds. Here every vertex has a dense
//! [`NodeId`] in `0..n` (`node_of` / `entity_of`), so "iterate all vertices" is
//! well defined and visits them in ascending entity-id order — the intended
//! upstream semantics. An entity that is in the dictionary but in no
//! entity–entity edge has no node: graph functions treat it as an isolated
//! vertex (no neighbours), as upstream's `getOutSize`/`getInSize` would.
//!
//! # Predicate filters
//!
//! [`PredFilter`] is upstream's resolved `pred_id_set`
//! (`GeneralEvaluation::getPredSetByStringList`, GE.cpp ~4165-4206): the empty
//! set `{}` means every predicate, a positive list means exactly those (unknown
//! IRIs dropped, duplicates dropped, first-appearance order kept), and any `!<p>`
//! exclusion means "every predicate except the excluded ones" — positive
//! entries are then *ignored*, as upstream does.
//!
//! Traversal visits predicates in the filter's order (ascending predicate id
//! for `All`/`Exclude`, list order for `Include`) and, within a predicate, the
//! neighbours in ascending id — the iteration order of upstream's
//! `for (int pred : pred_set) for (i < getOutSize(v, pred)) getOutVertID(v,
//! pred, i)` loops, which decides which of several equal-length paths upstream
//! reports.

use std::fmt;
use std::ops::{ControlFlow, Range};

use crate::dict::Dictionary;
use crate::error::{GStoreError, Result};
use crate::model::id::{is_entity_id, EntityLiteralId, PredId};
use crate::store::TripleSource;

/// A dense vertex index in `0..CsrGraph::num_nodes()`.
pub type NodeId = u32;

/// Which adjacency a traversal step follows.
///
/// The combined directions interleave **per predicate**: for every predicate of
/// the filter (in filter order) first one direction's neighbours, then the
/// other's. That is the loop shape of upstream's undirected traversals
/// (`for pred: { out-neighbours; if (!directed) in-neighbours; }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dir {
    /// Out-neighbours (`csr[0]`): the objects of `node`'s triples.
    Out,
    /// In-neighbours (`csr[1]`): the subjects of triples whose object is `node`.
    In,
    /// Per predicate: out-neighbours, then in-neighbours.
    OutIn,
    /// Per predicate: in-neighbours, then out-neighbours (upstream
    /// `PathQueryHandler::BFS` and `bfsCount`'s order).
    InOut,
}

impl Dir {
    /// Forward step of a traversal honouring upstream's `directed` flag:
    /// [`Dir::Out`] when directed, otherwise [`Dir::OutIn`] (every edge is
    /// bidirectional; out first, as in `shortestPath`, `SSSP`, `PR`,
    /// `closenessCentrality`).
    pub fn forward(directed: bool) -> Dir {
        if directed {
            Dir::Out
        } else {
            Dir::OutIn
        }
    }

    /// Backward step (towards a source, e.g. the destination side of a
    /// bidirectional BFS): [`Dir::In`] when directed, else [`Dir::InOut`].
    pub fn backward(directed: bool) -> Dir {
        if directed {
            Dir::In
        } else {
            Dir::InOut
        }
    }
}

/// One adjacency (all out-edges or all in-edges) in node-major CSR form.
/// `node`'s entries are `offsets[node] .. offsets[node + 1]`, sorted by
/// `(pred, nbr)`; `preds` and `nbrs` are parallel arrays.
#[derive(Clone, Default)]
struct Adjacency {
    offsets: Vec<u32>,
    preds: Vec<PredId>,
    nbrs: Vec<NodeId>,
}

impl Adjacency {
    /// Build from `(key_node, pred, nbr_node)` edges by counting sort on the key
    /// node followed by a per-node sort on `(pred, nbr)`.
    fn build(n: usize, edges: &[(NodeId, PredId, NodeId)]) -> Adjacency {
        let mut offsets = vec![0u32; n + 1];
        for &(k, _, _) in edges {
            offsets[k as usize + 1] += 1;
        }
        for i in 0..n {
            offsets[i + 1] += offsets[i];
        }
        let mut cursor: Vec<u32> = offsets[..n].to_vec();
        let mut entries: Vec<(PredId, NodeId)> = vec![(0, 0); edges.len()];
        for &(k, p, v) in edges {
            let slot = &mut cursor[k as usize];
            entries[*slot as usize] = (p, v);
            *slot += 1;
        }
        for i in 0..n {
            entries[offsets[i] as usize..offsets[i + 1] as usize].sort_unstable();
        }
        let (preds, nbrs) = entries.into_iter().unzip();
        Adjacency {
            offsets,
            preds,
            nbrs,
        }
    }

    #[inline]
    fn range(&self, node: NodeId) -> Range<usize> {
        let i = node as usize;
        match (self.offsets.get(i), self.offsets.get(i + 1)) {
            (Some(&a), Some(&b)) => a as usize..b as usize,
            _ => 0..0,
        }
    }

    /// The entry range of `node`'s neighbours under `pred`.
    #[inline]
    fn pred_range(&self, node: NodeId, pred: PredId) -> Range<usize> {
        let r = self.range(node);
        let ps = &self.preds[r.clone()];
        let lo = ps.partition_point(|&p| p < pred);
        let hi = lo + ps[lo..].partition_point(|&p| p == pred);
        r.start + lo..r.start + hi
    }

    #[inline]
    fn slice(&self, node: NodeId, pred: PredId) -> &[NodeId] {
        &self.nbrs[self.pred_range(node, pred)]
    }

    fn heap_bytes(&self) -> usize {
        self.offsets.capacity() * 4 + self.preds.capacity() * 4 + self.nbrs.capacity() * 4
    }
}

/// The per-predicate, bidirectional, entity-only graph of a triple store
/// (upstream `CSR[2]`). Immutable once built; shared as `Arc<CsrGraph>` by
/// [`Database::csr`](crate::Database::csr).
///
/// See the [module docs](self) for the exact model.
#[derive(Clone, Default)]
pub struct CsrGraph {
    /// node -> entity id, strictly ascending.
    nodes: Vec<EntityLiteralId>,
    out: Adjacency,
    inc: Adjacency,
    /// Entity–entity edge count per predicate id (`csr[1].adjacency_list[p].size()`).
    pred_edges: Vec<u32>,
    /// Predicates with at least one entity–entity edge, ascending.
    edge_preds: Vec<PredId>,
    /// One past the largest predicate id of *any* triple of the source
    /// (literal-object triples included) — a lower bound for upstream's
    /// predicate-id space `pre_num`.
    pred_slots: u32,
    num_edges: u64,
}

impl fmt::Debug for CsrGraph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CsrGraph")
            .field("nodes", &self.nodes.len())
            .field("edges", &self.num_edges)
            .field("edge_predicates", &self.edge_preds.len())
            .field("pred_slots", &self.pred_slots)
            .finish()
    }
}

impl CsrGraph {
    /// Build the CSR of `source` (one full scan + sorts: `O(E log E)`).
    ///
    /// Errors only when the entity–entity edge count does not fit the compact
    /// `u32` offsets (more than `u32::MAX` edges).
    pub fn build<S: TripleSource + ?Sized>(source: &S) -> Result<CsrGraph> {
        let all = source.iter_all();
        let mut pred_slots: u32 = 0;
        let mut raw: Vec<(EntityLiteralId, PredId, EntityLiteralId)> = Vec::new();
        for t in &all {
            pred_slots = pred_slots.max(t.pred.saturating_add(1));
            if is_entity_id(t.sub) && is_entity_id(t.obj) {
                raw.push((t.sub, t.pred, t.obj));
            }
        }
        drop(all);
        Self::from_edges(raw, pred_slots)
    }

    /// Assemble from raw `(subject, predicate, object)` entity edges.
    /// Duplicate triples are collapsed (a store never holds duplicates).
    fn from_edges(
        mut raw: Vec<(EntityLiteralId, PredId, EntityLiteralId)>,
        pred_slots: u32,
    ) -> Result<CsrGraph> {
        raw.sort_unstable();
        raw.dedup();
        if raw.len() > u32::MAX as usize {
            return Err(GStoreError::Query(format!(
                "graph too large for the CSR: {} entity edges (limit {})",
                raw.len(),
                u32::MAX
            )));
        }
        let mut nodes: Vec<EntityLiteralId> = Vec::with_capacity(raw.len().min(1 << 20) * 2);
        for &(s, _, o) in &raw {
            nodes.push(s);
            nodes.push(o);
        }
        nodes.sort_unstable();
        nodes.dedup();
        nodes.shrink_to_fit();
        let node = |id: EntityLiteralId| -> NodeId {
            // Present by construction.
            nodes.binary_search(&id).unwrap_or_default() as NodeId
        };
        let max_pred = raw.iter().map(|&(_, p, _)| p).max();
        let mut pred_edges = vec![0u32; max_pred.map_or(0, |p| p as usize + 1)];
        let mut fwd: Vec<(NodeId, PredId, NodeId)> = Vec::with_capacity(raw.len());
        for &(s, p, o) in &raw {
            pred_edges[p as usize] += 1;
            fwd.push((node(s), p, node(o)));
        }
        drop(raw);
        let n = nodes.len();
        let out = Adjacency::build(n, &fwd);
        for e in fwd.iter_mut() {
            *e = (e.2, e.1, e.0);
        }
        let inc = Adjacency::build(n, &fwd);
        let num_edges = fwd.len() as u64;
        drop(fwd);
        let edge_preds = pred_edges
            .iter()
            .enumerate()
            .filter(|(_, &c)| c > 0)
            .map(|(p, _)| p as PredId)
            .collect();
        Ok(CsrGraph {
            nodes,
            out,
            inc,
            pred_edges,
            edge_preds,
            pred_slots: pred_slots.max(max_pred.map_or(0, |p| p + 1)),
            num_edges,
        })
    }

    /// Build directly from `(subject, predicate, object)` entity-id edges
    /// (tests, synthetic graphs). Edges with a literal endpoint are skipped.
    pub fn from_triples(edges: &[(EntityLiteralId, PredId, EntityLiteralId)]) -> Result<CsrGraph> {
        let pred_slots = edges.iter().map(|e| e.1.saturating_add(1)).max().unwrap_or(0);
        let raw = edges
            .iter()
            .copied()
            .filter(|&(s, _, o)| is_entity_id(s) && is_entity_id(o))
            .collect();
        Self::from_edges(raw, pred_slots)
    }

    // ---- sizes ----------------------------------------------------------

    /// Number of vertices `n` (upstream `getVertNum()` / `csr[1].n`).
    pub fn num_nodes(&self) -> usize {
        self.nodes.len()
    }

    /// Total number of entity–entity edges `m`, counted per predicate
    /// (upstream `getEdgeNum()` / `csr[1].m`).
    pub fn num_edges(&self) -> u64 {
        self.num_edges
    }

    /// Edges labelled by a predicate the filter admits (upstream
    /// `getSetEdgeNum(pred_set)`).
    pub fn num_edges_in(&self, filter: &PredFilter) -> u64 {
        match &filter.kind {
            FilterKind::Include(list) => list.iter().map(|&p| self.pred_edge_count(p)).sum(),
            _ => self
                .edge_preds
                .iter()
                .filter(|&&p| filter.allows(p))
                .map(|&p| self.pred_edge_count(p))
                .sum(),
        }
    }

    /// Entity–entity edges labelled `pred` (`csr[1].adjacency_list[pred].size()`).
    pub fn pred_edge_count(&self, pred: PredId) -> u64 {
        u64::from(self.pred_edges.get(pred as usize).copied().unwrap_or(0))
    }

    /// Predicates that label at least one entity–entity edge, ascending.
    pub fn predicates(&self) -> &[PredId] {
        &self.edge_preds
    }

    /// One past the largest predicate id of any source triple (literal-object
    /// triples included): a lower bound of upstream's predicate-id space.
    pub fn pred_slots(&self) -> u32 {
        self.pred_slots
    }

    /// Approximate heap footprint in bytes (upstream `CSR::sizeInBytes`).
    pub fn heap_bytes(&self) -> usize {
        self.nodes.capacity() * 4
            + self.out.heap_bytes()
            + self.inc.heap_bytes()
            + self.pred_edges.capacity() * 4
            + self.edge_preds.capacity() * 4
    }

    // ---- id mapping -----------------------------------------------------

    /// The node of an entity id, if that entity is in an entity–entity edge.
    pub fn node_of(&self, entity: EntityLiteralId) -> Option<NodeId> {
        self.nodes.binary_search(&entity).ok().map(|i| i as NodeId)
    }

    /// The entity id of a node.
    ///
    /// # Panics
    /// When `node >= num_nodes()`.
    pub fn entity_of(&self, node: NodeId) -> EntityLiteralId {
        self.nodes[node as usize]
    }

    /// Every vertex's entity id, ascending (index = node id).
    pub fn entities(&self) -> &[EntityLiteralId] {
        &self.nodes
    }

    /// All node ids, ascending (= ascending entity id).
    pub fn node_ids(&self) -> Range<NodeId> {
        0..self.nodes.len() as NodeId
    }

    // ---- per-predicate access (upstream getOutSize/getOutVertID) ----------

    /// Out-neighbours of `node` under exactly `pred`, ascending
    /// (`getOutVertID(v, pred, 0..getOutSize(v, pred))`).
    pub fn out_neighbors(&self, node: NodeId, pred: PredId) -> &[NodeId] {
        self.out.slice(node, pred)
    }

    /// In-neighbours of `node` under exactly `pred`, ascending
    /// (`getInVertID(v, pred, 0..getInSize(v, pred))`).
    pub fn in_neighbors(&self, node: NodeId, pred: PredId) -> &[NodeId] {
        self.inc.slice(node, pred)
    }

    /// Neighbours of `node` under exactly `pred` in one base direction
    /// (`Dir::Out` or `Dir::In`; a combined direction yields the out list
    /// followed by nothing — use [`try_for_each`](Self::try_for_each) instead).
    pub fn neighbors(&self, node: NodeId, pred: PredId, dir: Dir) -> &[NodeId] {
        match dir {
            Dir::In => self.in_neighbors(node, pred),
            _ => self.out_neighbors(node, pred),
        }
    }

    // ---- degrees ----------------------------------------------------------

    /// Out-degree under the filter (upstream `getSetOutSize(v, pred_set)`),
    /// counting one per (predicate, neighbour) edge.
    pub fn out_degree(&self, node: NodeId, filter: &PredFilter) -> usize {
        Self::degree_in_adj(&self.out, node, filter)
    }

    /// In-degree under the filter (upstream `getSetInSize(v, pred_set)`).
    pub fn in_degree(&self, node: NodeId, filter: &PredFilter) -> usize {
        Self::degree_in_adj(&self.inc, node, filter)
    }

    /// Degree in the traversal direction: out-degree when `directed`, else
    /// out-degree + in-degree (a self-loop counts twice, as upstream).
    pub fn degree(&self, node: NodeId, filter: &PredFilter, directed: bool) -> usize {
        let out = self.out_degree(node, filter);
        if directed {
            out
        } else {
            out + self.in_degree(node, filter)
        }
    }

    /// Unfiltered out-degree over every predicate (`getTotalOutSize(v, false)`).
    pub fn total_out_degree(&self, node: NodeId) -> usize {
        self.out.range(node).len()
    }

    /// Unfiltered in-degree over every predicate (`getTotalInSize(v, false)`).
    pub fn total_in_degree(&self, node: NodeId) -> usize {
        self.inc.range(node).len()
    }

    fn degree_in_adj(adj: &Adjacency, node: NodeId, filter: &PredFilter) -> usize {
        match &filter.kind {
            FilterKind::All => adj.range(node).len(),
            FilterKind::Include(list) => list.iter().map(|&p| adj.pred_range(node, p).len()).sum(),
            FilterKind::Exclude(_) => {
                let r = adj.range(node);
                adj.preds[r].iter().filter(|&&p| filter.allows(p)).count()
            }
        }
    }

    // ---- traversal ----------------------------------------------------------

    /// Visit `node`'s neighbours in `dir` under `filter`, in upstream order
    /// (filter predicate order; per predicate the first direction's neighbours
    /// ascending, then — for combined directions — the second's). The visitor
    /// gets `(neighbour, predicate, base_direction)` where `base_direction` is
    /// [`Dir::Out`] or [`Dir::In`]; returning `ControlFlow::Break` stops early.
    pub fn try_for_each<B, F>(&self, node: NodeId, filter: &PredFilter, dir: Dir, mut f: F) -> ControlFlow<B>
    where
        F: FnMut(NodeId, PredId, Dir) -> ControlFlow<B>,
    {
        match &filter.kind {
            FilterKind::Include(list) => {
                for &p in list {
                    self.visit_pred(node, p, dir, &mut f)?;
                }
                ControlFlow::Continue(())
            }
            _ => match dir {
                Dir::Out => Self::scan(&self.out, node, filter, Dir::Out, &mut f),
                Dir::In => Self::scan(&self.inc, node, filter, Dir::In, &mut f),
                Dir::OutIn | Dir::InOut => self.scan_both(node, filter, dir, &mut f),
            },
        }
    }

    /// [`try_for_each`](Self::try_for_each) over out-neighbours, without early exit.
    pub fn for_each_out<F: FnMut(NodeId, PredId)>(&self, node: NodeId, filter: &PredFilter, mut f: F) {
        let _ = self.try_for_each::<(), _>(node, filter, Dir::Out, |v, p, _| {
            f(v, p);
            ControlFlow::Continue(())
        });
    }

    /// [`try_for_each`](Self::try_for_each) over in-neighbours, without early exit.
    pub fn for_each_in<F: FnMut(NodeId, PredId)>(&self, node: NodeId, filter: &PredFilter, mut f: F) {
        let _ = self.try_for_each::<(), _>(node, filter, Dir::In, |v, p, _| {
            f(v, p);
            ControlFlow::Continue(())
        });
    }

    /// Both directions, per predicate out-neighbours then in-neighbours
    /// ([`Dir::OutIn`]), without early exit. Use
    /// [`try_for_each`](Self::try_for_each) with [`Dir::InOut`] for the other
    /// interleaving.
    pub fn for_each_both<F: FnMut(NodeId, PredId)>(&self, node: NodeId, filter: &PredFilter, mut f: F) {
        let _ = self.try_for_each::<(), _>(node, filter, Dir::OutIn, |v, p, _| {
            f(v, p);
            ControlFlow::Continue(())
        });
    }

    fn visit_pred<B, F>(&self, node: NodeId, p: PredId, dir: Dir, f: &mut F) -> ControlFlow<B>
    where
        F: FnMut(NodeId, PredId, Dir) -> ControlFlow<B>,
    {
        let (first, second) = match dir {
            Dir::Out => (Dir::Out, None),
            Dir::In => (Dir::In, None),
            Dir::OutIn => (Dir::Out, Some(Dir::In)),
            Dir::InOut => (Dir::In, Some(Dir::Out)),
        };
        for d in std::iter::once(first).chain(second) {
            let adj = if d == Dir::Out { &self.out } else { &self.inc };
            for &v in adj.slice(node, p) {
                f(v, p, d)?;
            }
        }
        ControlFlow::Continue(())
    }

    fn scan<B, F>(adj: &Adjacency, node: NodeId, filter: &PredFilter, d: Dir, f: &mut F) -> ControlFlow<B>
    where
        F: FnMut(NodeId, PredId, Dir) -> ControlFlow<B>,
    {
        let r = adj.range(node);
        for i in r {
            let p = adj.preds[i];
            if filter.allows(p) {
                f(adj.nbrs[i], p, d)?;
            }
        }
        ControlFlow::Continue(())
    }

    /// Merge the out- and in-entry runs of `node` predicate by predicate
    /// (ascending), emitting each predicate's runs in `dir`'s order.
    fn scan_both<B, F>(&self, node: NodeId, filter: &PredFilter, dir: Dir, f: &mut F) -> ControlFlow<B>
    where
        F: FnMut(NodeId, PredId, Dir) -> ControlFlow<B>,
    {
        let ro = self.out.range(node);
        let ri = self.inc.range(node);
        let (mut io, mut ii) = (ro.start, ri.start);
        while io < ro.end || ii < ri.end {
            let po = if io < ro.end { self.out.preds[io] } else { PredId::MAX };
            let pi = if ii < ri.end { self.inc.preds[ii] } else { PredId::MAX };
            let p = po.min(pi);
            let eo = if po == p {
                io + self.out.preds[io..ro.end].partition_point(|&x| x == p)
            } else {
                io
            };
            let ei = if pi == p {
                ii + self.inc.preds[ii..ri.end].partition_point(|&x| x == p)
            } else {
                ii
            };
            if filter.allows(p) {
                let runs = [(&self.out, io..eo, Dir::Out), (&self.inc, ii..ei, Dir::In)];
                let order: [usize; 2] = if dir == Dir::OutIn { [0, 1] } else { [1, 0] };
                for k in order {
                    let (adj, r, d) = (&runs[k].0, runs[k].1.clone(), runs[k].2);
                    for j in r {
                        f(adj.nbrs[j], p, d)?;
                    }
                }
            }
            io = eo;
            ii = ei;
        }
        ControlFlow::Continue(())
    }
}

// ---------------------------------------------------------------------------
// PredFilter
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum FilterKind {
    /// `{}`: every predicate, ascending id.
    All,
    /// A positive list (deduplicated, first-appearance order, unknown IRIs dropped).
    Include(Vec<PredId>),
    /// Every predicate except these (sorted, deduplicated). Unknown IRIs are
    /// simply absent.
    Exclude(Vec<PredId>),
}

/// A resolved predicate set (upstream `pred_id_set`). See the
/// [module docs](self) for the semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredFilter {
    kind: FilterKind,
    /// Size of the predicate-id space at resolve time (upstream
    /// `stringindex->getNum(Predicate)`), used to materialise `All`/`Exclude`.
    pred_space: u32,
}

impl Default for PredFilter {
    fn default() -> Self {
        PredFilter::all(0)
    }
}

impl PredFilter {
    /// Every predicate (`{}`). `pred_space` is the size of the predicate-id
    /// space (the dictionary's predicate count); it only matters for
    /// [`pred_ids`](Self::pred_ids) / [`is_empty`](Self::is_empty).
    pub fn all(pred_space: u32) -> PredFilter {
        PredFilter {
            kind: FilterKind::All,
            pred_space,
        }
    }

    /// Exactly these predicate ids, in this order (duplicates dropped).
    pub fn include(ids: impl IntoIterator<Item = PredId>) -> PredFilter {
        let mut seen = std::collections::HashSet::new();
        let list: Vec<PredId> = ids.into_iter().filter(|p| seen.insert(*p)).collect();
        PredFilter {
            kind: FilterKind::Include(list),
            pred_space: 0,
        }
    }

    /// Every predicate except these.
    pub fn exclude(ids: impl IntoIterator<Item = PredId>, pred_space: u32) -> PredFilter {
        let mut list: Vec<PredId> = ids.into_iter().collect();
        list.sort_unstable();
        list.dedup();
        PredFilter {
            kind: FilterKind::Exclude(list),
            pred_space,
        }
    }

    /// Resolve a parsed predicate set against the dictionary with upstream
    /// semantics (`getPredSetByStringList`, GE.cpp ~4165-4206):
    ///
    /// * `include` and `exclude` both empty → every predicate;
    /// * only `include` → those predicates that exist, deduplicated, in
    ///   first-appearance order (an all-unknown list resolves to the *empty*
    ///   set: no edge qualifies);
    /// * any `exclude` → every predicate except the excluded ones; `include`
    ///   is ignored and unknown excluded IRIs are ignored.
    ///
    /// IRIs may be given bare (`http://x/p`) or in dictionary-key form
    /// (`<http://x/p>`).
    pub fn resolve<S: AsRef<str>>(include: &[S], exclude: &[S], dict: &Dictionary) -> PredFilter {
        let space = u32::try_from(dict.predicate_num()).unwrap_or(u32::MAX);
        let lookup = |iri: &str| dict.predicate_id(&pred_key(iri));
        if exclude.is_empty() {
            if include.is_empty() {
                PredFilter::all(space)
            } else {
                PredFilter::include(include.iter().filter_map(|s| lookup(s.as_ref())))
            }
        } else {
            PredFilter::exclude(exclude.iter().filter_map(|s| lookup(s.as_ref())), space)
        }
    }

    /// Does the filter admit predicate `p`?
    #[inline]
    pub fn allows(&self, p: PredId) -> bool {
        match &self.kind {
            FilterKind::All => true,
            FilterKind::Include(list) => list.contains(&p),
            FilterKind::Exclude(list) => list.binary_search(&p).is_err(),
        }
    }

    /// Whether this is the unrestricted `{}` filter.
    pub fn is_all(&self) -> bool {
        matches!(self.kind, FilterKind::All)
    }

    /// The explicit positive list, for an include filter.
    pub fn included(&self) -> Option<&[PredId]> {
        match &self.kind {
            FilterKind::Include(l) => Some(l),
            _ => None,
        }
    }

    /// Upstream's materialised `pred_id_set` vector, in its exact order:
    /// ascending ids of the predicate space minus exclusions for `All` /
    /// `Exclude`, the list itself for `Include`. The space is the larger of
    /// the resolve-time dictionary count and `csr`'s [`pred_slots`]
    /// (`CsrGraph::pred_slots`), so every predicate with an edge is covered.
    pub fn pred_ids(&self, csr: &CsrGraph) -> Vec<PredId> {
        match &self.kind {
            FilterKind::Include(l) => l.clone(),
            _ => {
                let space = self.pred_space.max(csr.pred_slots());
                (0..space).filter(|&p| self.allows(p)).collect()
            }
        }
    }

    /// Whether upstream's `pred_id_set` would be empty (several functions
    /// special-case that: `triangleCounting`/`clusterCoeff` return -1,
    /// `betweennessCentrality`/`degreeCorrelation` return 0).
    pub fn is_empty(&self, csr: &CsrGraph) -> bool {
        match &self.kind {
            FilterKind::Include(l) => l.is_empty(),
            FilterKind::All => self.pred_space.max(csr.pred_slots()) == 0,
            FilterKind::Exclude(ex) => {
                let space = self.pred_space.max(csr.pred_slots());
                (0..space).all(|p| ex.binary_search(&p).is_ok())
            }
        }
    }
}

/// Dictionary key of a predicate IRI given bare or bracketed.
fn pred_key(iri: &str) -> String {
    if iri.starts_with('<') && iri.ends_with('>') {
        iri.to_string()
    } else {
        format!("<{iri}>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edges_of(g: &CsrGraph, node: NodeId, f: &PredFilter, dir: Dir) -> Vec<(NodeId, PredId, Dir)> {
        let mut v = Vec::new();
        let _ = g.try_for_each::<(), _>(node, f, dir, |n, p, d| {
            v.push((n, p, d));
            ControlFlow::Continue(())
        });
        v
    }

    #[test]
    fn combined_directions_interleave_per_predicate() {
        // 10 -0-> 11, 10 -1-> 12, 13 -0-> 10, 13 -1-> 10
        let g = CsrGraph::from_triples(&[(10, 0, 11), (10, 1, 12), (13, 0, 10), (13, 1, 10)]).unwrap();
        let a = g.node_of(10).unwrap();
        let all = PredFilter::all(2);
        let (n11, n12, n13) = (g.node_of(11).unwrap(), g.node_of(12).unwrap(), g.node_of(13).unwrap());
        assert_eq!(
            edges_of(&g, a, &all, Dir::OutIn),
            vec![(n11, 0, Dir::Out), (n13, 0, Dir::In), (n12, 1, Dir::Out), (n13, 1, Dir::In)]
        );
        assert_eq!(
            edges_of(&g, a, &all, Dir::InOut),
            vec![(n13, 0, Dir::In), (n11, 0, Dir::Out), (n13, 1, Dir::In), (n12, 1, Dir::Out)]
        );
        // Include order wins over id order.
        let inc = PredFilter::include([1, 0]);
        assert_eq!(
            edges_of(&g, a, &inc, Dir::OutIn),
            vec![(n12, 1, Dir::Out), (n13, 1, Dir::In), (n11, 0, Dir::Out), (n13, 0, Dir::In)]
        );
        // Early exit.
        let mut seen = 0;
        let r = g.try_for_each(a, &all, Dir::OutIn, |_, _, _| {
            seen += 1;
            if seen == 2 {
                ControlFlow::Break(7)
            } else {
                ControlFlow::Continue(())
            }
        });
        assert_eq!(r, ControlFlow::Break(7));
        assert_eq!(seen, 2);
    }

    #[test]
    fn exclude_filter_scans_and_degrees() {
        let g = CsrGraph::from_triples(&[(1, 0, 2), (1, 1, 3), (1, 2, 4)]).unwrap();
        let n1 = g.node_of(1).unwrap();
        let ex = PredFilter::exclude([1], 3);
        assert_eq!(g.out_degree(n1, &ex), 2);
        assert_eq!(g.out_degree(n1, &PredFilter::all(3)), 3);
        assert_eq!(g.degree(n1, &ex, false), 2);
        assert_eq!(g.num_edges_in(&ex), 2);
        assert_eq!(ex.pred_ids(&g), vec![0, 2]);
        assert!(!ex.is_empty(&g));
        assert!(PredFilter::exclude([0, 1, 2], 3).is_empty(&g));
        assert!(PredFilter::include(Vec::<PredId>::new()).is_empty(&g));
    }
}
