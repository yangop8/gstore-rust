//! Per-predicate CSR (compressed sparse row) graph — the structure gStore's
//! advanced graph functions (`shortestPath`, `PR`, `WCC`, …) run on.
//!
//! # Upstream model
//!
//! Mirrors gStore 1.4's `CSR` (`src/Database/CSR.{h,cpp}`) as built by
//! `GeneralEvaluation::loadCSR` (`src/Query/GeneralEvaluation.cpp:137-258`)
//! and read through `CSRQueryHandler` (`src/Query/Algorithm/CSRQueryHandler.cpp`):
//!
//! * **Entities only.** A triple `(s, p, o)` becomes an edge `s -p-> o` only when
//!   `o` is an entity (IRI / blank node); triples with a literal object are
//!   skipped (GE:173-177 drops objects `>= LITERAL_FIRST_ID`; GE:210-211 skips
//!   literal objects for the in-direction).
//! * **Per predicate, both directions.** Upstream keeps `csr[0]` (out: subject →
//!   objects) and `csr[1]` (in: object → subjects) *per predicate*. Here every
//!   node has one out- and one in-adjacency run sorted by `(predicate,
//!   neighbour)`, so `out_nbrs(v, p)` is exactly upstream's
//!   `getOutVertID(v, p, 0..getOutSize(v, p))` (ascending neighbour id — the
//!   KVstore lists the loop reads are sorted).
//! * **Multi-edges across predicates are kept.** `(a, p1, b)` and `(a, p2, b)`
//!   are two edges, one per predicate, exactly like upstream (the same `(s, o)`
//!   pair appears once in each predicate's list). Within one predicate a triple
//!   is a set element, so there are no duplicates.
//! * **Vertex set / counts.** [`CsrGraph::num_nodes`] is upstream `csr[1].n`
//!   (distinct vertices appearing in any adjacency list, GE:238-245) and
//!   [`CsrGraph::num_edges`] is `csr[1].m` (sum of per-predicate edge counts,
//!   GE:247-250).
//!
//! # Node ids
//!
//! Upstream addresses vertices by their raw entity id and many algorithms loop
//! `for (vid = 0; vid < getVertNum(); ++vid)`, i.e. they assume entity ids are
//! dense. Here vertices are *dense node indices* `0..num_nodes()` assigned in
//! **ascending entity-id order** ([`CsrGraph::node_of`] /
//! [`CsrGraph::entity_of`]). When the CSR's entity ids are exactly
//! `0..n` (the common case: every entity takes part in an entity→entity edge)
//! node index == entity id and iteration orders coincide with upstream; when
//! they are not, upstream's loops skip / mis-index vertices (undefined
//! behaviour in places) while ours visit every CSR vertex once — a deliberate,
//! documented deviation. Because node order is monotone in entity id, sorting
//! by node index equals sorting by entity id (what upstream's `std::set<int>` /
//! `std::map<int, …>` iteration produces).
//!
//! # Named graphs
//!
//! Upstream has no named graphs. The CSR is built from the **default graph**
//! only; triples in named graphs are invisible to graph functions.
//!
//! # Predicate filters
//!
//! [`PredFilter`] is a resolved `predSet` argument (`{}`, `{<p1>, <p2>}`,
//! `{!<p>}`) with upstream's semantics (`GeneralEvaluation::getPredSetByStringList`,
//! GE:4181-4216): an *ordered* list of predicate ids. The order matters —
//! upstream algorithms iterate `for (int pred : pred_set)` and the first
//! path / neighbour found depends on it — so every traversal helper here
//! iterates predicates in the filter's order.

use std::fmt;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::dict::Dictionary;
use crate::error::{GStoreError, Result};
use crate::model::id::{is_entity_id, EntityLiteralId, PredId, INVALID_PRED_ID};
use crate::model::IdTriple;
use crate::store::TripleSource;

/// A dense CSR vertex index in `0..CsrGraph::num_nodes()`.
pub type Node = u32;

/// Which incident edges of a vertex a traversal follows.
///
/// gStore's `directed` argument maps to [`Direction::from_directed`]:
/// `true` → follow out-edges only, `false` → treat every edge as
/// bidirectional (out- and in-edges).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Out-edges only (`getOutSize` / `getOutVertID`).
    Out,
    /// In-edges only (`getInSize` / `getInVertID`).
    In,
    /// Out- and in-edges (an undirected view).
    Both,
}

impl Direction {
    /// gStore's `directed` flag: `true` → [`Direction::Out`], `false` →
    /// [`Direction::Both`].
    pub fn from_directed(directed: bool) -> Direction {
        if directed {
            Direction::Out
        } else {
            Direction::Both
        }
    }

    /// The opposite traversal direction (`Out` ↔ `In`; `Both` stays `Both`).
    /// Backward searches (e.g. the `v` side of a bidirectional BFS) use it.
    pub fn reverse(self) -> Direction {
        match self {
            Direction::Out => Direction::In,
            Direction::In => Direction::Out,
            Direction::Both => Direction::Both,
        }
    }
}

/// The direction of one concrete edge relative to the vertex being expanded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdgeDir {
    /// The edge leaves the expanded vertex (`v -p-> nbr`).
    Out,
    /// The edge enters the expanded vertex (`nbr -p-> v`).
    In,
}

// ---------------------------------------------------------------------------
// PredFilter
// ---------------------------------------------------------------------------

/// A resolved `predSet` argument: the ordered list of predicate ids a graph
/// function may traverse (upstream `pred_id_set`).
///
/// Resolution ([`PredFilter::resolve`]) follows
/// `GeneralEvaluation::getPredSetByStringList` (GE:4181-4216) exactly:
///
/// | written            | include | exclude | resolved list                                              |
/// |--------------------|---------|---------|------------------------------------------------------------|
/// | `{}`               | empty   | empty   | every predicate id of the dictionary, ascending            |
/// | `{<p1>, <p2>}`     | `p1,p2` | empty   | `p1, p2` in the written order, de-duplicated; IRIs unknown to the dictionary are dropped (so the list may be **empty**) |
/// | `{!<p1>}`, `{<p2>, !<p1>}` | any | non-empty | every predicate id ascending **except** the excluded ones; the include list is **ignored** whenever an exclusion is present; unknown excluded IRIs are ignored |
///
/// An empty resolved list is meaningful: traversals see no edges and
/// functions such as `triangleCounting` / `clusterCoeff` return their
/// "no predicates" value (`-1`).
///
/// [`Default`] is the empty list (no predicate allowed).
#[derive(Clone, PartialEq, Eq, Default)]
pub struct PredFilter {
    /// The ordered, de-duplicated predicate ids.
    preds: Vec<PredId>,
    /// Whether `preds` is strictly ascending (enables linear adjacency scans
    /// that produce the same order as per-predicate lookups).
    ascending: bool,
    /// Membership bitmap indexed by predicate id.
    bits: Vec<u64>,
}

impl fmt::Debug for PredFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PredFilter")
            .field("preds", &self.preds)
            .finish()
    }
}

impl PredFilter {
    /// A filter from explicit predicate ids, keeping their order and dropping
    /// repeats (first occurrence wins, like upstream's `temp_set` dedup).
    /// [`INVALID_PRED_ID`] entries are dropped.
    pub fn from_ids<I: IntoIterator<Item = PredId>>(ids: I) -> PredFilter {
        let mut f = PredFilter {
            preds: Vec::new(),
            ascending: true,
            bits: Vec::new(),
        };
        for p in ids {
            if p == INVALID_PRED_ID || f.contains(p) {
                continue;
            }
            if let Some(&last) = f.preds.last() {
                if p <= last {
                    f.ascending = false;
                }
            }
            let word = (p / 64) as usize;
            if f.bits.len() <= word {
                f.bits.resize(word + 1, 0);
            }
            f.bits[word] |= 1u64 << (p % 64);
            f.preds.push(p);
        }
        f
    }

    /// Every predicate id the dictionary knows, ascending (upstream: the
    /// empty `{}` set → `0..pre_num`).
    pub fn all(dict: &Dictionary) -> PredFilter {
        PredFilter::from_ids(dictionary_predicate_ids(dict))
    }

    /// Resolve a written `predSet` against the dictionary with upstream
    /// semantics (see the type docs). `include` / `exclude` are predicate
    /// IRIs; each may be written bare (`http://x/p`) or in angle brackets
    /// (`<http://x/p>`), the latter being the dictionary key form.
    pub fn resolve<S: AsRef<str>>(include: &[S], exclude: &[S], dict: &Dictionary) -> PredFilter {
        if include.is_empty() && exclude.is_empty() {
            return PredFilter::all(dict);
        }
        if exclude.is_empty() {
            return PredFilter::from_ids(
                include
                    .iter()
                    .filter_map(|iri| dict.predicate_id(&predicate_key(iri.as_ref()))),
            );
        }
        let excluded = PredFilter::from_ids(
            exclude
                .iter()
                .filter_map(|iri| dict.predicate_id(&predicate_key(iri.as_ref()))),
        );
        PredFilter::from_ids(
            dictionary_predicate_ids(dict)
                .into_iter()
                .filter(|&p| !excluded.contains(p)),
        )
    }

    /// The ordered predicate ids (upstream `pred_id_set`).
    pub fn preds(&self) -> &[PredId] {
        &self.preds
    }

    /// Number of predicates in the list (upstream `pred_set.size()`).
    pub fn len(&self) -> usize {
        self.preds.len()
    }

    /// Whether no predicate is allowed (upstream `pred_set.empty()`).
    pub fn is_empty(&self) -> bool {
        self.preds.is_empty()
    }

    /// Whether `p` is in the list.
    pub fn contains(&self, p: PredId) -> bool {
        self.bits
            .get((p / 64) as usize)
            .is_some_and(|w| w & (1u64 << (p % 64)) != 0)
    }

    /// Whether the list is strictly ascending (true for `{}` and exclusion
    /// sets). Traversals then scan adjacency runs linearly instead of looking
    /// each predicate up, with an identical visiting order.
    pub fn is_ascending(&self) -> bool {
        self.ascending
    }
}

/// The dictionary key of a predicate IRI written either bare or bracketed.
fn predicate_key(iri: &str) -> String {
    if iri.starts_with('<') && iri.ends_with('>') {
        iri.to_string()
    } else {
        format!("<{iri}>")
    }
}

/// Every live predicate id of `dict`, ascending. Freed (reclaimed) ids leave
/// holes, so the id space is scanned until all `predicate_num()` live ids
/// were found.
fn dictionary_predicate_ids(dict: &Dictionary) -> Vec<PredId> {
    let want = dict.predicate_num();
    let mut out = Vec::with_capacity(want);
    let mut id: PredId = 0;
    while out.len() < want && id < INVALID_PRED_ID {
        if dict.predicate_to_string(id).is_some() {
            out.push(id);
        }
        id += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// CsrGraph
// ---------------------------------------------------------------------------

/// One direction of adjacency: node `v`'s edges are
/// `pred[off[v]..off[v+1]]` / `nbr[off[v]..off[v+1]]`, sorted by
/// `(pred, nbr)`.
#[derive(Clone, Default)]
struct Adjacency {
    off: Vec<u32>,
    pred: Vec<PredId>,
    nbr: Vec<Node>,
}

impl Adjacency {
    #[inline]
    fn range(&self, v: Node) -> std::ops::Range<usize> {
        let v = v as usize;
        match (self.off.get(v), self.off.get(v + 1)) {
            (Some(&a), Some(&b)) => a as usize..b as usize,
            _ => 0..0,
        }
    }

    #[inline]
    fn edges(&self, v: Node) -> (&[PredId], &[Node]) {
        let r = self.range(v);
        (&self.pred[r.clone()], &self.nbr[r])
    }

    /// The neighbours of `v` via predicate `p` (ascending).
    #[inline]
    fn slice(&self, v: Node, p: PredId) -> &[Node] {
        let (preds, nbrs) = self.edges(v);
        let lo = preds.partition_point(|&x| x < p);
        let hi = lo + preds[lo..].partition_point(|&x| x <= p);
        &nbrs[lo..hi]
    }

    fn bytes(&self) -> usize {
        (self.off.len() + self.pred.len() + self.nbr.len()) * 4
    }
}

/// The per-predicate, bidirectional, entity-only CSR of a triple store (see
/// the module docs for the exact upstream correspondence).
///
/// Immutable once built; share it behind an [`Arc`] (see [`CsrCache`]).
#[derive(Clone, Default)]
pub struct CsrGraph {
    /// `entities[node]` = the node's entity id; strictly ascending.
    entities: Vec<EntityLiteralId>,
    out: Adjacency,
    inc: Adjacency,
    /// Distinct predicates labelling at least one edge, ascending.
    preds: Vec<PredId>,
    /// `pred_edges[i]` = number of edges labelled `preds[i]`.
    pred_edges: Vec<u64>,
    num_edges: u64,
}

impl fmt::Debug for CsrGraph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CsrGraph")
            .field("nodes", &self.num_nodes())
            .field("edges", &self.num_edges)
            .field("predicates", &self.preds.len())
            .finish()
    }
}

impl CsrGraph {
    /// An empty graph (no vertices, no edges).
    pub fn empty() -> CsrGraph {
        CsrGraph {
            out: Adjacency {
                off: vec![0],
                ..Adjacency::default()
            },
            inc: Adjacency {
                off: vec![0],
                ..Adjacency::default()
            },
            ..CsrGraph::default()
        }
    }

    /// Build the CSR of every triple `src` holds (one `iter_all` scan, then two
    /// sorts: `O(E log E)`). Triples with a literal object are skipped.
    ///
    /// Errors only if the graph exceeds the compact `u32` index space
    /// (more than `u32::MAX` vertices or edges).
    pub fn build<S: TripleSource + ?Sized>(src: &S) -> Result<CsrGraph> {
        CsrGraph::from_id_triples(src.iter_all())
    }

    /// Build the CSR from raw id-triples (literal-object triples are skipped,
    /// duplicates collapsed). The building block of [`build`](Self::build),
    /// public for tests and for callers holding triples already.
    pub fn from_id_triples(mut triples: Vec<IdTriple>) -> Result<CsrGraph> {
        triples.retain(|t| is_entity_id(t.sub) && is_entity_id(t.obj));
        if triples.is_empty() {
            return Ok(CsrGraph::empty());
        }
        if triples.len() > u32::MAX as usize {
            return Err(GStoreError::Database(format!(
                "graph too large for the CSR: {} edges (limit {})",
                triples.len(),
                u32::MAX
            )));
        }

        // Vertex set: every subject and object, ascending.
        let mut entities: Vec<EntityLiteralId> = Vec::with_capacity(triples.len() * 2);
        for t in &triples {
            entities.push(t.sub);
            entities.push(t.obj);
        }
        entities.sort_unstable();
        entities.dedup();
        entities.shrink_to_fit();
        let n = entities.len();
        // Entity → node lookup for the build: a direct table when the ids are
        // reasonably dense (the usual case: ids are allocated sequentially),
        // a binary search otherwise. Every endpoint is in `entities`.
        let max_id = entities.last().copied().unwrap_or(0) as usize;
        let dense: Option<Vec<Node>> = (max_id < 4 * n + 1024).then(|| {
            let mut table = vec![Node::MAX; max_id + 1];
            for (i, &e) in entities.iter().enumerate() {
                table[e as usize] = i as Node;
            }
            table
        });
        let node = |e: EntityLiteralId| -> Node {
            match &dense {
                Some(table) => table[e as usize],
                None => entities.binary_search(&e).map_or(0, |i| i as Node),
            }
        };

        // Out-adjacency: sorted by (sub, pred, obj); node order is monotone in
        // entity id, so this is also (node(sub), pred, node(obj)) order.
        triples.sort_unstable();
        triples.dedup();
        let m = triples.len();
        let mut out = Adjacency {
            off: vec![0u32; n + 1],
            pred: Vec::with_capacity(m),
            nbr: Vec::with_capacity(m),
        };
        let mut pred_count: std::collections::BTreeMap<PredId, u64> = Default::default();
        for t in &triples {
            out.off[node(t.sub) as usize + 1] += 1;
            out.pred.push(t.pred);
            out.nbr.push(node(t.obj));
            *pred_count.entry(t.pred).or_insert(0) += 1;
        }
        for i in 0..n {
            out.off[i + 1] += out.off[i];
        }

        // In-adjacency: sorted by (obj, pred, sub).
        triples.sort_unstable_by_key(|t| (t.obj, t.pred, t.sub));
        let mut inc = Adjacency {
            off: vec![0u32; n + 1],
            pred: Vec::with_capacity(m),
            nbr: Vec::with_capacity(m),
        };
        for t in &triples {
            inc.off[node(t.obj) as usize + 1] += 1;
            inc.pred.push(t.pred);
            inc.nbr.push(node(t.sub));
        }
        for i in 0..n {
            inc.off[i + 1] += inc.off[i];
        }
        drop(triples);

        let (preds, pred_edges): (Vec<PredId>, Vec<u64>) = pred_count.into_iter().unzip();
        Ok(CsrGraph {
            entities,
            out,
            inc,
            preds,
            pred_edges,
            num_edges: m as u64,
        })
    }

    // ---- sizes --------------------------------------------------------------

    /// Number of vertices (upstream `getVertNum()` = `csr[1].n`): distinct
    /// entities incident to at least one entity→entity edge.
    pub fn num_nodes(&self) -> usize {
        self.entities.len()
    }

    /// Total number of edges over all predicates, counting an `(s, o)` pair
    /// once per predicate (upstream `getEdgeNum()` = `csr[1].m`).
    pub fn num_edges(&self) -> u64 {
        self.num_edges
    }

    /// Number of edges labelled by a predicate of `filter` (upstream
    /// `getSetEdgeNum(pred_set)`; a predicate listed twice is counted once
    /// because filters are de-duplicated).
    pub fn num_edges_with(&self, filter: &PredFilter) -> u64 {
        filter
            .preds()
            .iter()
            .map(|&p| self.pred_edge_count(p))
            .sum()
    }

    /// Number of edges labelled `p` (0 for a predicate without entity edges).
    pub fn pred_edge_count(&self, p: PredId) -> u64 {
        self.preds
            .binary_search(&p)
            .map_or(0, |i| self.pred_edges[i])
    }

    /// The predicates labelling at least one edge, ascending.
    pub fn predicates(&self) -> &[PredId] {
        &self.preds
    }

    /// Approximate heap footprint in bytes (upstream `CSR::sizeInBytes`).
    pub fn memory_bytes(&self) -> usize {
        self.entities.len() * 4
            + self.out.bytes()
            + self.inc.bytes()
            + self.preds.len() * 4
            + self.pred_edges.len() * 8
    }

    // ---- id mapping ---------------------------------------------------------

    /// The CSR node of an entity id, if the entity takes part in an
    /// entity→entity edge (`O(log n)`).
    pub fn node_of(&self, entity: EntityLiteralId) -> Option<Node> {
        self.entities.binary_search(&entity).ok().map(|i| i as Node)
    }

    /// The entity id of a CSR node.
    ///
    /// # Panics
    /// If `node >= num_nodes()`.
    pub fn entity_of(&self, node: Node) -> EntityLiteralId {
        self.entities[node as usize]
    }

    /// All vertices' entity ids, indexed by node (strictly ascending).
    pub fn entities(&self) -> &[EntityLiteralId] {
        &self.entities
    }

    // ---- adjacency ----------------------------------------------------------

    /// Out-neighbours of `node` via predicate `pred`, ascending (upstream
    /// `getOutVertID(v, pred, 0..getOutSize(v, pred))`). Empty for an unknown
    /// node or predicate.
    pub fn out_nbrs(&self, node: Node, pred: PredId) -> &[Node] {
        self.out.slice(node, pred)
    }

    /// In-neighbours of `node` via predicate `pred`, ascending (upstream
    /// `getInVertID(v, pred, 0..getInSize(v, pred))`).
    pub fn in_nbrs(&self, node: Node, pred: PredId) -> &[Node] {
        self.inc.slice(node, pred)
    }

    /// Neighbours of `node` via `pred` in one edge direction.
    pub fn nbrs(&self, node: Node, pred: PredId, dir: EdgeDir) -> &[Node] {
        match dir {
            EdgeDir::Out => self.out_nbrs(node, pred),
            EdgeDir::In => self.in_nbrs(node, pred),
        }
    }

    /// All out-edges of `node` as parallel `(predicates, neighbours)` slices,
    /// sorted by `(predicate, neighbour)`.
    pub fn out_edges(&self, node: Node) -> (&[PredId], &[Node]) {
        self.out.edges(node)
    }

    /// All in-edges of `node` as parallel `(predicates, neighbours)` slices,
    /// sorted by `(predicate, neighbour)`.
    pub fn in_edges(&self, node: Node) -> (&[PredId], &[Node]) {
        self.inc.edges(node)
    }

    /// Visit the out-neighbours of `node` allowed by `filter`: for each
    /// predicate in filter order, its neighbours ascending.
    pub fn for_each_out<F: FnMut(Node, PredId)>(&self, node: Node, filter: &PredFilter, mut f: F) {
        let _ = self.try_for_each_dir(&self.out, node, filter, |n, p| {
            f(n, p);
            ControlFlow::<()>::Continue(())
        });
    }

    /// Visit the in-neighbours of `node` allowed by `filter` (same order rule
    /// as [`for_each_out`](Self::for_each_out)).
    pub fn for_each_in<F: FnMut(Node, PredId)>(&self, node: Node, filter: &PredFilter, mut f: F) {
        let _ = self.try_for_each_dir(&self.inc, node, filter, |n, p| {
            f(n, p);
            ControlFlow::<()>::Continue(())
        });
    }

    /// Visit out- and in-neighbours of `node` allowed by `filter` in the
    /// interleaving most upstream algorithms use: **for each predicate** (in
    /// filter order) its out-neighbours, then its in-neighbours. The closure
    /// also receives the edge direction. (For the other upstream orders see
    /// `gfun::traverse::NbrOrder`.)
    pub fn for_each_both<F: FnMut(Node, PredId, EdgeDir)>(
        &self,
        node: Node,
        filter: &PredFilter,
        mut f: F,
    ) {
        let _ = self.try_for_each_both(node, filter, |n, p, d| {
            f(n, p, d);
            ControlFlow::<()>::Continue(())
        });
    }

    /// Early-exit form of [`for_each_out`](Self::for_each_out).
    pub fn try_for_each_out<B, F: FnMut(Node, PredId) -> ControlFlow<B>>(
        &self,
        node: Node,
        filter: &PredFilter,
        f: F,
    ) -> ControlFlow<B> {
        self.try_for_each_dir(&self.out, node, filter, f)
    }

    /// Early-exit form of [`for_each_in`](Self::for_each_in).
    pub fn try_for_each_in<B, F: FnMut(Node, PredId) -> ControlFlow<B>>(
        &self,
        node: Node,
        filter: &PredFilter,
        f: F,
    ) -> ControlFlow<B> {
        self.try_for_each_dir(&self.inc, node, filter, f)
    }

    /// Early-exit form of [`for_each_both`](Self::for_each_both).
    pub fn try_for_each_both<B, F: FnMut(Node, PredId, EdgeDir) -> ControlFlow<B>>(
        &self,
        node: Node,
        filter: &PredFilter,
        mut f: F,
    ) -> ControlFlow<B> {
        if filter.is_ascending() {
            // Merge the two predicate-sorted runs group by group.
            let (op, on) = self.out.edges(node);
            let (ip, inn) = self.inc.edges(node);
            let (mut i, mut j) = (0usize, 0usize);
            while i < op.len() || j < ip.len() {
                let p = match (op.get(i), ip.get(j)) {
                    (Some(&a), Some(&b)) => a.min(b),
                    (Some(&a), None) => a,
                    (None, Some(&b)) => b,
                    (None, None) => break,
                };
                let i_end = i + op[i..].partition_point(|&x| x <= p);
                let j_end = j + ip[j..].partition_point(|&x| x <= p);
                if filter.contains(p) {
                    for &n in &on[i..i_end] {
                        f(n, p, EdgeDir::Out)?;
                    }
                    for &n in &inn[j..j_end] {
                        f(n, p, EdgeDir::In)?;
                    }
                }
                i = i_end;
                j = j_end;
            }
        } else {
            for &p in filter.preds() {
                for &n in self.out.slice(node, p) {
                    f(n, p, EdgeDir::Out)?;
                }
                for &n in self.inc.slice(node, p) {
                    f(n, p, EdgeDir::In)?;
                }
            }
        }
        ControlFlow::Continue(())
    }

    fn try_for_each_dir<B, F: FnMut(Node, PredId) -> ControlFlow<B>>(
        &self,
        adj: &Adjacency,
        node: Node,
        filter: &PredFilter,
        mut f: F,
    ) -> ControlFlow<B> {
        if filter.is_ascending() {
            let (preds, nbrs) = adj.edges(node);
            for (&p, &n) in preds.iter().zip(nbrs) {
                if filter.contains(p) {
                    f(n, p)?;
                }
            }
        } else {
            for &p in filter.preds() {
                for &n in adj.slice(node, p) {
                    f(n, p)?;
                }
            }
        }
        ControlFlow::Continue(())
    }

    // ---- degrees ------------------------------------------------------------

    /// Out-degree of `node` over `filter`, counting parallel edges of
    /// different predicates separately (upstream `getSetOutSize`).
    pub fn out_degree(&self, node: Node, filter: &PredFilter) -> usize {
        self.dir_degree(&self.out, node, filter)
    }

    /// In-degree of `node` over `filter` (upstream `getSetInSize`).
    pub fn in_degree(&self, node: Node, filter: &PredFilter) -> usize {
        self.dir_degree(&self.inc, node, filter)
    }

    /// Degree in a [`Direction`]; `Both` = out + in (with multiplicity).
    pub fn degree(&self, node: Node, filter: &PredFilter, dir: Direction) -> usize {
        match dir {
            Direction::Out => self.out_degree(node, filter),
            Direction::In => self.in_degree(node, filter),
            Direction::Both => self.out_degree(node, filter) + self.in_degree(node, filter),
        }
    }

    fn dir_degree(&self, adj: &Adjacency, node: Node, filter: &PredFilter) -> usize {
        if filter.is_ascending() {
            adj.edges(node)
                .0
                .iter()
                .filter(|&&p| filter.contains(p))
                .count()
        } else {
            filter
                .preds()
                .iter()
                .map(|&p| adj.slice(node, p).len())
                .sum()
        }
    }

    /// Distinct neighbours of `node` over `filter` in a [`Direction`],
    /// ascending (upstream's `std::set<int>` neighbour sets, e.g.
    /// `getTotalOutSize(v, true)` / `getOutVertID(v, pos)` over all predicates).
    pub fn distinct_neighbors(&self, node: Node, filter: &PredFilter, dir: Direction) -> Vec<Node> {
        let mut v = Vec::new();
        if matches!(dir, Direction::Out | Direction::Both) {
            self.for_each_out(node, filter, |n, _| v.push(n));
        }
        if matches!(dir, Direction::In | Direction::Both) {
            self.for_each_in(node, filter, |n, _| v.push(n));
        }
        v.sort_unstable();
        v.dedup();
        v
    }
}

// ---------------------------------------------------------------------------
// CsrCache
// ---------------------------------------------------------------------------

/// A thread-safe, lazily (re)built `Arc<CsrGraph>` keyed by a data
/// generation counter (e.g. [`Database::mutation_count`](crate::Database::mutation_count)).
///
/// [`get_or_build`](Self::get_or_build) returns the cached graph when it was
/// built for the same generation and rebuilds it otherwise — upstream's
/// `prepPathQuery` (GE:262-271: rebuild when `kvstore->isCSRUpdate()`), but
/// with the dirty flag replaced by the generation so no write path has to
/// remember to set it. Concurrent callers wait for an in-flight build instead
/// of building twice.
#[derive(Default)]
pub struct CsrCache {
    slot: Mutex<Option<(u64, Arc<CsrGraph>)>>,
    /// Whether `slot` holds a graph, readable without its lock (which a
    /// build holds) — see [`status`](Self::status).
    built: CsrStatus,
}

/// Whether a [`CsrCache`] holds a graph, readable without waiting — not
/// for a build in progress, nor for whoever holds the owner (e.g. a server's
/// per-database lock, held while a statement runs). A clone shares the
/// state; it reads `false` once the cache is dropped.
#[derive(Debug, Clone, Default)]
pub struct CsrStatus(Arc<AtomicBool>);

impl CsrStatus {
    /// Whether the cache holds a graph (as of the last completed build or
    /// clear; `false` while one is being rebuilt).
    pub fn is_built(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn set(&self, built: bool) {
        self.0.store(built, Ordering::Release);
    }
}

impl fmt::Debug for CsrCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let slot = self.lock();
        match slot.as_ref() {
            Some((gen, g)) => write!(f, "CsrCache {{ generation: {gen}, graph: {g:?} }}"),
            None => write!(f, "CsrCache {{ empty }}"),
        }
    }
}

impl CsrCache {
    /// An empty cache.
    pub fn new() -> CsrCache {
        CsrCache::default()
    }

    /// A cache already holding `graph` for `generation` — e.g. an immutable
    /// read snapshot reusing the CSR its source database had cached for the
    /// same data.
    pub fn seeded(generation: u64, graph: Arc<CsrGraph>) -> CsrCache {
        let built = CsrStatus::default();
        built.set(true);
        CsrCache {
            slot: Mutex::new(Some((generation, graph))),
            built,
        }
    }

    /// A lock-free view of whether this cache holds a graph (see
    /// [`CsrStatus`]). For a cache keyed by a counter that is cleared
    /// whenever the counter advances — [`Database`](crate::Database)'s —
    /// that is whether a graph for the current generation is cached.
    pub fn status(&self) -> CsrStatus {
        self.built.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<(u64, Arc<CsrGraph>)>> {
        // The slot is a plain value; a panic during a build leaves it valid.
        self.slot.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The graph for `generation`: the cached one if it was built for that
    /// generation, otherwise `build()`'s result (which is then cached). An
    /// entry for another generation is dropped *before* the build, so the
    /// old and the new graph are never both held by the cache (upstream
    /// also resets its CSR before rebuilding, GeneralEvaluation.cpp:141-143);
    /// a failed build leaves the cache empty.
    pub fn get_or_build<F>(&self, generation: u64, build: F) -> Result<Arc<CsrGraph>>
    where
        F: FnOnce() -> Result<CsrGraph>,
    {
        let mut slot = self.lock();
        if let Some((gen, g)) = slot.as_ref() {
            if *gen == generation {
                return Ok(Arc::clone(g));
            }
        }
        *slot = None;
        self.built.set(false);
        let g = Arc::new(build()?);
        *slot = Some((generation, Arc::clone(&g)));
        self.built.set(true);
        Ok(g)
    }

    /// The cached graph if it is current for `generation` (never builds).
    pub fn peek(&self, generation: u64) -> Option<Arc<CsrGraph>> {
        match self.lock().as_ref() {
            Some((gen, g)) if *gen == generation => Some(Arc::clone(g)),
            _ => None,
        }
    }

    /// The generation the cached graph was built for, if any.
    pub fn cached_generation(&self) -> Option<u64> {
        self.lock().as_ref().map(|(gen, _)| *gen)
    }

    /// Drop the cached graph.
    pub fn clear(&self) {
        let mut slot = self.lock();
        *slot = None;
        self.built.set(false);
    }
}

impl Drop for CsrCache {
    fn drop(&mut self) {
        // A status kept by someone else must not report a dropped graph.
        self.built.set(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: u32, p: u32, o: u32) -> IdTriple {
        IdTriple::new(s, p, o)
    }

    #[test]
    fn literal_objects_are_skipped_and_nodes_are_ascending() {
        let lit = crate::model::id::LITERAL_FIRST_ID + 3;
        let g = CsrGraph::from_id_triples(vec![t(5, 0, 2), t(2, 1, 9), t(7, 0, lit)]).unwrap();
        assert_eq!(g.entities(), &[2, 5, 9]);
        assert_eq!(g.num_nodes(), 3);
        assert_eq!(g.num_edges(), 2);
        assert_eq!(
            g.node_of(7),
            None,
            "subject with only literal objects is not a vertex"
        );
        assert_eq!(g.node_of(9), Some(2));
        assert_eq!(g.entity_of(1), 5);
    }

    #[test]
    fn sparse_and_dense_entity_ids_build_the_same_shape() {
        // Sparse ids (binary-search lookup during the build) …
        let sparse = CsrGraph::from_id_triples(vec![
            t(1_000_000, 0, 7),
            t(7, 0, 1_999_999_999),
            t(1_000_000, 1, 1_999_999_999),
        ])
        .unwrap();
        // … and the same graph with dense ids (table lookup).
        let dense = CsrGraph::from_id_triples(vec![t(1, 0, 0), t(0, 0, 2), t(1, 1, 2)]).unwrap();
        assert_eq!(sparse.entities(), &[7, 1_000_000, 1_999_999_999]);
        for g in [&sparse, &dense] {
            assert_eq!(g.num_nodes(), 3);
            assert_eq!(g.out_nbrs(1, 0), &[0]);
            assert_eq!(g.out_nbrs(1, 1), &[2]);
            assert_eq!(g.out_nbrs(0, 0), &[2]);
            assert_eq!(g.in_nbrs(2, 0), &[0]);
            assert_eq!(g.in_nbrs(2, 1), &[1]);
        }
    }

    #[test]
    fn slices_follow_predicate_then_neighbour_order() {
        // 0 -p1-> {3, 1}, 0 -p0-> {2}
        let g = CsrGraph::from_id_triples(vec![t(0, 1, 3), t(0, 0, 2), t(0, 1, 1)]).unwrap();
        let n0 = g.node_of(0).unwrap();
        let ent = |ns: &[Node]| ns.iter().map(|&n| g.entity_of(n)).collect::<Vec<_>>();
        assert_eq!(ent(g.out_nbrs(n0, 1)), vec![1, 3]);
        assert_eq!(ent(g.out_nbrs(n0, 0)), vec![2]);
        assert!(g.out_nbrs(n0, 7).is_empty());
        let mut seen = Vec::new();
        g.for_each_out(n0, &PredFilter::from_ids([1, 0]), |n, p| {
            seen.push((p, g.entity_of(n)))
        });
        assert_eq!(seen, vec![(1, 1), (1, 3), (0, 2)]);
        seen.clear();
        g.for_each_out(n0, &PredFilter::from_ids([0, 1]), |n, p| {
            seen.push((p, g.entity_of(n)))
        });
        assert_eq!(seen, vec![(0, 2), (1, 1), (1, 3)]);
    }

    #[test]
    fn both_interleaves_per_predicate_in_both_scan_modes() {
        // a=0, b=1, c=2: a -p0-> b, c -p0-> a, a -p1-> c, b -p1-> a
        let g = CsrGraph::from_id_triples(vec![t(0, 0, 1), t(2, 0, 0), t(0, 1, 2), t(1, 1, 0)])
            .unwrap();
        let a = g.node_of(0).unwrap();
        let collect = |f: &PredFilter| {
            let mut v = Vec::new();
            g.for_each_both(a, f, |n, p, d| v.push((p, g.entity_of(n), d)));
            v
        };
        let asc = collect(&PredFilter::from_ids([0, 1]));
        assert_eq!(
            asc,
            vec![
                (0, 1, EdgeDir::Out),
                (0, 2, EdgeDir::In),
                (1, 2, EdgeDir::Out),
                (1, 1, EdgeDir::In)
            ]
        );
        let desc = collect(&PredFilter::from_ids([1, 0]));
        assert_eq!(
            desc,
            vec![
                (1, 2, EdgeDir::Out),
                (1, 1, EdgeDir::In),
                (0, 1, EdgeDir::Out),
                (0, 2, EdgeDir::In)
            ]
        );
        assert_eq!(g.degree(a, &PredFilter::from_ids([1]), Direction::Both), 2);
        assert_eq!(g.out_degree(a, &PredFilter::from_ids([0, 1])), 2);
    }

    #[test]
    fn try_variants_stop_early() {
        let g = CsrGraph::from_id_triples(vec![t(0, 0, 1), t(0, 0, 2), t(0, 0, 3)]).unwrap();
        let mut n = 0;
        let r = g.try_for_each_out(0, &PredFilter::from_ids([0]), |x, _| {
            n += 1;
            if x == 2 {
                ControlFlow::Break(x)
            } else {
                ControlFlow::Continue(())
            }
        });
        assert_eq!(r, ControlFlow::Break(2));
        assert_eq!(n, 2);
    }

    #[test]
    fn pred_filter_from_ids_dedups_and_tracks_order() {
        let f = PredFilter::from_ids([3, 1, 3, INVALID_PRED_ID, 200]);
        assert_eq!(f.preds(), &[3, 1, 200]);
        assert!(!f.is_ascending());
        assert!(f.contains(200) && f.contains(1) && !f.contains(2));
        assert!(PredFilter::from_ids([1, 5, 9]).is_ascending());
        assert!(PredFilter::default().is_empty());
    }

    #[test]
    fn cache_rebuilds_only_on_new_generation() {
        let cache = CsrCache::new();
        let mut builds = 0;
        let g1 = cache
            .get_or_build(1, || {
                builds += 1;
                Ok(CsrGraph::empty())
            })
            .unwrap();
        let g2 = cache.get_or_build(1, || unreachable!()).unwrap();
        assert!(Arc::ptr_eq(&g1, &g2));
        assert!(cache.peek(2).is_none());
        let g3 = cache
            .get_or_build(2, || {
                builds += 1;
                Ok(CsrGraph::empty())
            })
            .unwrap();
        assert!(!Arc::ptr_eq(&g1, &g3));
        assert_eq!(builds, 2);
        assert_eq!(cache.cached_generation(), Some(2));
        // A rebuild drops the stale entry first: the build no longer sees
        // it, and a failed build leaves the cache empty.
        let weak = Arc::downgrade(&g3);
        drop((g1, g2, g3));
        assert!(cache
            .get_or_build(3, || {
                assert!(weak.upgrade().is_none(), "stale CSR alive during the build");
                Err(GStoreError::Database("x".into()))
            })
            .is_err());
        assert_eq!(cache.cached_generation(), None);
        cache.get_or_build(3, || Ok(CsrGraph::empty())).unwrap();
        assert_eq!(cache.cached_generation(), Some(3));
        cache.clear();
        assert_eq!(cache.cached_generation(), None);
    }
}
