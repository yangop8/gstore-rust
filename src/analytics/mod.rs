//! Graph analytics for the entity graph embedded in the RDF triple store.
//!
//! Mirrors gStore's `src/Query/topk` / gpstore graph-computation layer, which
//! computes graph metrics directly over the id-space entity graph.  Predicates
//! are ignored; every triple (s, p, o) is projected to a directed edge s → o.
//!
//! The central type is [`GraphView`], which builds a compact CSR
//! (Compressed-Sparse-Row) adjacency representation from a [`TripleStore`] and
//! exposes graph algorithms over the entity-id space.
//!
//! # Deferred work
//! * Betweenness / closeness centrality
//! * Louvain community detection
//! * Strongly-connected components (Tarjan / Kosaraju)
//! * Weighted / labeled edge variants (predicate-aware)
//! * Top-k subgraph-proximity queries (gStore `topk` module)

use std::collections::{HashMap, VecDeque};

use crate::model::id::EntityLiteralId;
use crate::store::TripleStore;

// ---------------------------------------------------------------------------
// Union-find helpers (iterative path-halving + union-by-rank)
// ---------------------------------------------------------------------------

/// Iterative path-halving find; avoids the recursive mutable-borrow conflict
/// that full path compression would create in safe Rust.
fn uf_find(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        let px = parent[x];
        parent[x] = parent[px]; // point x to its grandparent
        x = parent[x];
    }
    x
}

/// Union-by-rank: merge the sets containing `a` and `b`.
fn uf_union(parent: &mut [usize], rank: &mut [u8], a: usize, b: usize) {
    let ra = uf_find(parent, a);
    let rb = uf_find(parent, b);
    if ra == rb {
        return;
    }
    match rank[ra].cmp(&rank[rb]) {
        std::cmp::Ordering::Less => parent[ra] = rb,
        std::cmp::Ordering::Greater => parent[rb] = ra,
        std::cmp::Ordering::Equal => {
            parent[rb] = ra;
            rank[ra] += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// GraphView
// ---------------------------------------------------------------------------

/// A compact directed-graph view of the entity graph embedded in a
/// [`TripleStore`].
///
/// Sparse entity ids are mapped to a contiguous dense index space `[0, n)` for
/// algorithmic efficiency.  All public results are returned keyed by the
/// original entity ids.
///
/// Construction is O(E log E) (edge sort + dedup).  BFS and PageRank are
/// O(V + E); WCC is O(E α(V)) (near-linear); triangle count is O(E · d_max).
pub struct GraphView {
    /// Dense-to-sparse: `nodes[dense_idx] = EntityLiteralId`.
    nodes: Vec<EntityLiteralId>,
    /// Sparse-to-dense index lookup.
    id_to_idx: HashMap<EntityLiteralId, usize>,
    /// CSR out-adjacency offsets: node `i`'s out-neighbours are
    /// `out_adj[out_ptr[i]..out_ptr[i+1]]`.
    out_ptr: Vec<usize>,
    /// CSR out-adjacency targets (dense indices).
    out_adj: Vec<usize>,
    /// CSR in-adjacency offsets.
    in_ptr: Vec<usize>,
    /// CSR in-adjacency sources (dense indices).
    in_adj: Vec<usize>,
}

impl GraphView {
    /// Build a `GraphView` from a triple store.
    ///
    /// Every triple `(s, p, o)` becomes a directed edge `s → o`; predicates
    /// are ignored.  When two triples share the same (subject, object) pair but
    /// differ in predicate, the edge is de-duplicated so each `(s, o)` pair
    /// appears at most once.
    pub fn from_store(store: &TripleStore) -> Self {
        // Collect raw (sub, obj) pairs from every triple in one pass.
        let raw_pairs: Vec<(EntityLiteralId, EntityLiteralId)> =
            store.iter_all().map(|t| (t.sub, t.obj)).collect();

        // 1. Build sorted, de-duplicated node list.
        let mut id_set: Vec<EntityLiteralId> =
            raw_pairs.iter().flat_map(|&(s, o)| [s, o]).collect();
        id_set.sort_unstable();
        id_set.dedup();
        let n = id_set.len();

        // 2. Sparse → dense lookup.
        let id_to_idx: HashMap<EntityLiteralId, usize> =
            id_set.iter().copied().enumerate().map(|(i, id)| (id, i)).collect();

        // 3. De-duplicate directed edges (multiple predicates can share (s,o)).
        let mut raw_edges: Vec<(usize, usize)> = raw_pairs
            .iter()
            .map(|&(s, o)| (id_to_idx[&s], id_to_idx[&o]))
            .collect();
        raw_edges.sort_unstable();
        raw_edges.dedup();
        let m = raw_edges.len();

        // 4. Count out- and in-degrees.
        let mut out_deg = vec![0usize; n];
        let mut in_deg = vec![0usize; n];
        for &(s, o) in &raw_edges {
            out_deg[s] += 1;
            in_deg[o] += 1;
        }

        // 5. Build CSR out-adjacency.
        let mut out_ptr = vec![0usize; n + 1];
        for i in 0..n {
            out_ptr[i + 1] = out_ptr[i] + out_deg[i];
        }
        let mut out_adj = vec![0usize; m];
        {
            let mut cur = out_ptr[..n].to_vec(); // cur[i] = next write position for node i
            for &(s, o) in &raw_edges {
                out_adj[cur[s]] = o;
                cur[s] += 1;
            }
        }

        // 6. Build CSR in-adjacency.
        let mut in_ptr = vec![0usize; n + 1];
        for i in 0..n {
            in_ptr[i + 1] = in_ptr[i] + in_deg[i];
        }
        let mut in_adj = vec![0usize; m];
        {
            let mut cur = in_ptr[..n].to_vec();
            for &(s, o) in &raw_edges {
                in_adj[cur[o]] = s;
                cur[o] += 1;
            }
        }

        GraphView { nodes: id_set, id_to_idx, out_ptr, out_adj, in_ptr, in_adj }
    }

    /// Number of nodes in the graph.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of directed edges (after de-duplication across predicates).
    pub fn edge_count(&self) -> usize {
        self.out_adj.len()
    }

    // ---- degree -----------------------------------------------------------

    /// Out-degree of `id`: number of distinct entity targets reachable by a
    /// single directed edge.  Returns `None` if `id` is not a node.
    pub fn out_degree(&self, id: EntityLiteralId) -> Option<usize> {
        let i = *self.id_to_idx.get(&id)?;
        Some(self.out_ptr[i + 1] - self.out_ptr[i])
    }

    /// In-degree of `id`: number of distinct entity sources with a directed
    /// edge to `id`.  Returns `None` if `id` is not a node.
    pub fn in_degree(&self, id: EntityLiteralId) -> Option<usize> {
        let i = *self.id_to_idx.get(&id)?;
        Some(self.in_ptr[i + 1] - self.in_ptr[i])
    }

    /// Out-degree for every node: `entity_id → out_degree`.
    pub fn all_out_degrees(&self) -> HashMap<EntityLiteralId, usize> {
        self.nodes
            .iter()
            .enumerate()
            .map(|(i, &id)| (id, self.out_ptr[i + 1] - self.out_ptr[i]))
            .collect()
    }

    /// In-neighbours of `id`: entity ids with a directed edge to `id`.
    /// Returns `None` if `id` is not a node.
    pub fn in_neighbors(&self, id: EntityLiteralId) -> Option<Vec<EntityLiteralId>> {
        let i = *self.id_to_idx.get(&id)?;
        let srcs = self.in_adj[self.in_ptr[i]..self.in_ptr[i + 1]]
            .iter()
            .map(|&d| self.nodes[d])
            .collect();
        Some(srcs)
    }

    /// In-degree for every node: `entity_id → in_degree`.
    pub fn all_in_degrees(&self) -> HashMap<EntityLiteralId, usize> {
        self.nodes
            .iter()
            .enumerate()
            .map(|(i, &id)| (id, self.in_ptr[i + 1] - self.in_ptr[i]))
            .collect()
    }

    // ---- BFS --------------------------------------------------------------

    /// Single-source BFS distances from `src` over directed edges.
    ///
    /// Returns a map `entity_id → distance` for every node reachable from
    /// `src` (including `src` itself at distance 0).  Returns `None` if `src`
    /// is not in the graph.
    pub fn bfs_distances(&self, src: EntityLiteralId) -> Option<HashMap<EntityLiteralId, u32>> {
        let src_i = *self.id_to_idx.get(&src)?;
        let n = self.nodes.len();
        let mut dist = vec![u32::MAX; n];
        dist[src_i] = 0;
        let mut queue = VecDeque::new();
        queue.push_back(src_i);
        while let Some(u) = queue.pop_front() {
            let d = dist[u];
            for &v in &self.out_adj[self.out_ptr[u]..self.out_ptr[u + 1]] {
                if dist[v] == u32::MAX {
                    dist[v] = d + 1;
                    queue.push_back(v);
                }
            }
        }
        Some(
            self.nodes
                .iter()
                .enumerate()
                .filter(|&(i, _)| dist[i] != u32::MAX)
                .map(|(i, &id)| (id, dist[i]))
                .collect(),
        )
    }

    /// Shortest directed path from `src` to `dst` as an ordered list of entity
    /// ids (inclusive of both endpoints).
    ///
    /// Returns `None` if either node is absent or no directed path exists.
    /// Returns `Some([src])` when `src == dst`.
    pub fn shortest_path(
        &self,
        src: EntityLiteralId,
        dst: EntityLiteralId,
    ) -> Option<Vec<EntityLiteralId>> {
        let src_i = *self.id_to_idx.get(&src)?;
        let dst_i = *self.id_to_idx.get(&dst)?;

        if src_i == dst_i {
            return Some(vec![src]);
        }

        let n = self.nodes.len();
        let mut prev = vec![usize::MAX; n];
        let mut visited = vec![false; n];
        visited[src_i] = true;
        let mut queue = VecDeque::new();
        queue.push_back(src_i);
        'bfs: while let Some(u) = queue.pop_front() {
            for &v in &self.out_adj[self.out_ptr[u]..self.out_ptr[u + 1]] {
                if !visited[v] {
                    visited[v] = true;
                    prev[v] = u;
                    if v == dst_i {
                        break 'bfs;
                    }
                    queue.push_back(v);
                }
            }
        }
        if !visited[dst_i] {
            return None;
        }
        // Reconstruct path by walking prev[] back from dst to src.
        let mut path = Vec::new();
        let mut cur = dst_i;
        while cur != src_i {
            path.push(self.nodes[cur]);
            cur = prev[cur];
        }
        path.push(self.nodes[src_i]);
        path.reverse();
        Some(path)
    }

    // ---- Weakly connected components --------------------------------------

    /// Weakly-connected component (WCC) membership for every node.
    ///
    /// Uses union-find (union-by-rank + iterative path-halving) over the
    /// undirected view of the graph.  Two nodes `u` and `v` are in the same
    /// WCC iff `comp_map[&u] == comp_map[&v]`.
    ///
    /// Returns `(comp_map, component_count)` where `comp_map` maps each entity
    /// id to the id of a representative node in the same WCC.  Which node is
    /// chosen as representative is an implementation detail; only equality of
    /// representatives is meaningful.
    pub fn weakly_connected_components(
        &self,
    ) -> (HashMap<EntityLiteralId, EntityLiteralId>, usize) {
        let n = self.nodes.len();
        let mut parent: Vec<usize> = (0..n).collect();
        let mut rank = vec![0u8; n];

        // Union over the undirected graph: each directed edge (u→v) merges WCCs.
        for u in 0..n {
            for &v in &self.out_adj[self.out_ptr[u]..self.out_ptr[u + 1]] {
                uf_union(&mut parent, &mut rank, u, v);
            }
        }

        // Assign a canonical representative (the entity id of the UF root) to
        // each component, counting distinct components as we encounter new roots.
        let mut root_to_rep: HashMap<usize, EntityLiteralId> = HashMap::new();
        let mut num_components = 0usize;
        let mut comp_map: HashMap<EntityLiteralId, EntityLiteralId> =
            HashMap::with_capacity(n);
        for i in 0..n {
            let root = uf_find(&mut parent, i);
            let rep = *root_to_rep.entry(root).or_insert_with(|| {
                num_components += 1;
                self.nodes[root]
            });
            comp_map.insert(self.nodes[i], rep);
        }

        (comp_map, num_components)
    }

    // ---- PageRank ---------------------------------------------------------

    /// PageRank scores for all nodes.
    ///
    /// Runs the standard iterative PageRank with `damping` factor (typically
    /// 0.85).  Iterates until the L₁ change drops below `tol` or `max_iters`
    /// is exhausted.  Dangling nodes (out-degree 0) spread their rank uniformly
    /// across all nodes so that rank is conserved.
    ///
    /// Returns `entity_id → score`; scores sum to approximately 1.0.
    pub fn pagerank(
        &self,
        damping: f64,
        max_iters: usize,
        tol: f64,
    ) -> HashMap<EntityLiteralId, f64> {
        let n = self.nodes.len();
        if n == 0 {
            return HashMap::new();
        }
        let init = 1.0 / n as f64;
        let mut rank = vec![init; n];
        let mut next = vec![0.0f64; n];
        let out_deg: Vec<usize> = (0..n)
            .map(|i| self.out_ptr[i + 1] - self.out_ptr[i])
            .collect();

        for _ in 0..max_iters {
            // Collect rank from dangling nodes for uniform redistribution.
            let dangling_sum: f64 =
                (0..n).filter(|&i| out_deg[i] == 0).map(|i| rank[i]).sum();

            // Teleportation base received by every node.
            let base = (1.0 - damping + damping * dangling_sum) / n as f64;
            for v in next.iter_mut() {
                *v = base;
            }
            // Propagate rank along directed edges.
            for u in 0..n {
                if out_deg[u] > 0 {
                    let contrib = damping * rank[u] / out_deg[u] as f64;
                    for &v in &self.out_adj[self.out_ptr[u]..self.out_ptr[u + 1]] {
                        next[v] += contrib;
                    }
                }
            }
            // Convergence check.
            let delta: f64 =
                rank.iter().zip(next.iter()).map(|(a, b)| (a - b).abs()).sum();
            std::mem::swap(&mut rank, &mut next);
            if delta < tol {
                break;
            }
        }

        self.nodes.iter().enumerate().map(|(i, &id)| (id, rank[i])).collect()
    }

    // ---- Triangle count (undirected) --------------------------------------

    /// Count undirected triangles in the graph.
    ///
    /// Builds a sorted undirected adjacency list (self-loops excluded, parallel
    /// edges de-duplicated), then for each edge `(u, v)` with dense `u < v`
    /// counts common neighbours via sorted-merge intersection.  The raw
    /// intersection total equals three times the triangle count (each triangle
    /// appears once per edge), so the result is divided by three.
    pub fn triangle_count(&self) -> u64 {
        let n = self.nodes.len();
        // Build undirected adjacency (dense indices, no self-loops).
        let mut udj: Vec<Vec<usize>> = vec![Vec::new(); n];
        for u in 0..n {
            for &v in &self.out_adj[self.out_ptr[u]..self.out_ptr[u + 1]] {
                if u != v {
                    udj[u].push(v);
                    udj[v].push(u);
                }
            }
        }
        for adj in udj.iter_mut() {
            adj.sort_unstable();
            adj.dedup();
        }

        // For each edge (u, v) with u < v, count |N(u) ∩ N(v)|.
        // Each triangle {a, b, c} is counted exactly 3 times — divide by 3.
        let mut raw = 0u64;
        for u in 0..n {
            for &v in &udj[u] {
                if v <= u {
                    continue; // process each undirected edge once
                }
                let (au, av) = (&udj[u], &udj[v]);
                let (mut i, mut j) = (0, 0);
                while i < au.len() && j < av.len() {
                    match au[i].cmp(&av[j]) {
                        std::cmp::Ordering::Less => i += 1,
                        std::cmp::Ordering::Greater => j += 1,
                        std::cmp::Ordering::Equal => {
                            raw += 1;
                            i += 1;
                            j += 1;
                        }
                    }
                }
            }
        }
        raw / 3
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::IdTriple;
    use crate::store::TripleStore;

    /// Three-component test graph:
    ///
    /// ```text
    /// Component A (path):      10 → 11 → 12 → 13
    /// Component B (triangle):  20 → 21 → 22 → 20
    /// Component C (isolated):  30 → 30  (self-loop — appears as a single node)
    /// ```
    fn make_store() -> TripleStore {
        let mut s = TripleStore::new();
        let p = 1u32;
        s.insert(IdTriple::new(10, p, 11));
        s.insert(IdTriple::new(11, p, 12));
        s.insert(IdTriple::new(12, p, 13));
        s.insert(IdTriple::new(20, p, 21));
        s.insert(IdTriple::new(21, p, 22));
        s.insert(IdTriple::new(22, p, 20));
        s.insert(IdTriple::new(30, p, 30)); // self-loop places 30 in the graph
        s
    }

    #[test]
    fn test_degrees() {
        let store = make_store();
        let g = GraphView::from_store(&store);

        // Path: tail (10) has out=1, in=0; head (13) has out=0, in=1.
        assert_eq!(g.out_degree(10), Some(1));
        assert_eq!(g.in_degree(10), Some(0));
        assert_eq!(g.out_degree(11), Some(1));
        assert_eq!(g.in_degree(11), Some(1));
        assert_eq!(g.out_degree(13), Some(0));
        assert_eq!(g.in_degree(13), Some(1));

        // Triangle: each node has exactly one outgoing and one incoming edge.
        assert_eq!(g.out_degree(20), Some(1));
        assert_eq!(g.in_degree(20), Some(1)); // ← from 22

        // Self-loop on 30: counts as out=1, in=1.
        assert_eq!(g.out_degree(30), Some(1));
        assert_eq!(g.in_degree(30), Some(1));

        // Non-existent node.
        assert_eq!(g.out_degree(999), None);
        assert_eq!(g.in_degree(999), None);
    }

    #[test]
    fn test_bfs_distances() {
        let store = make_store();
        let g = GraphView::from_store(&store);

        let dists = g.bfs_distances(10).expect("node 10 must be in graph");
        assert_eq!(dists[&10], 0);
        assert_eq!(dists[&11], 1);
        assert_eq!(dists[&12], 2);
        assert_eq!(dists[&13], 3);
        // Triangle and isolated node are unreachable from 10.
        assert!(!dists.contains_key(&20));
        assert!(!dists.contains_key(&30));

        // From inside the triangle (directed 3-cycle).
        let d20 = g.bfs_distances(20).expect("node 20 must be in graph");
        assert_eq!(d20[&20], 0);
        assert_eq!(d20[&21], 1);
        assert_eq!(d20[&22], 2);
    }

    #[test]
    fn test_shortest_path() {
        let store = make_store();
        let g = GraphView::from_store(&store);

        let path = g.shortest_path(10, 13).expect("directed path 10→13 exists");
        assert_eq!(path, vec![10, 11, 12, 13]);

        // Trivial same-node path.
        assert_eq!(g.shortest_path(10, 10), Some(vec![10]));

        // No path from path component to triangle component (disconnected).
        assert!(g.shortest_path(10, 20).is_none());

        // No reverse-direction path (directed graph).
        assert!(g.shortest_path(13, 10).is_none());
    }

    #[test]
    fn test_weakly_connected_components() {
        let store = make_store();
        let g = GraphView::from_store(&store);

        let (comp_map, num_components) = g.weakly_connected_components();

        // Expect exactly three WCCs: {10,11,12,13}, {20,21,22}, {30}.
        assert_eq!(num_components, 3);

        // All path nodes share the same representative.
        let rep_path = comp_map[&10];
        assert_eq!(comp_map[&11], rep_path);
        assert_eq!(comp_map[&12], rep_path);
        assert_eq!(comp_map[&13], rep_path);

        // All triangle nodes share the same representative.
        let rep_tri = comp_map[&20];
        assert_eq!(comp_map[&21], rep_tri);
        assert_eq!(comp_map[&22], rep_tri);

        // Isolated node 30 is its own component.
        let rep_iso = comp_map[&30];
        assert_ne!(rep_iso, rep_path);
        assert_ne!(rep_iso, rep_tri);
    }

    #[test]
    fn test_pagerank_ordering() {
        // Hub graph: spokes 10, 11, 12 all point to hub 5; hub points back to
        // spoke 10.  Node 5 receives 3 in-edges and should rank highest.
        let mut s = TripleStore::new();
        let p = 1u32;
        s.insert(IdTriple::new(10, p, 5));
        s.insert(IdTriple::new(11, p, 5));
        s.insert(IdTriple::new(12, p, 5));
        s.insert(IdTriple::new(5, p, 10)); // hub has one out-edge

        let g = GraphView::from_store(&s);
        let pr = g.pagerank(0.85, 100, 1e-8);

        let score_5 = pr[&5];
        assert!(score_5 > pr[&10], "hub must outrank spoke 10");
        assert!(score_5 > pr[&11], "hub must outrank dangling spoke 11");
        assert!(score_5 > pr[&12], "hub must outrank dangling spoke 12");
    }

    #[test]
    fn test_triangle_count() {
        let store = make_store();
        let g = GraphView::from_store(&store);
        // Path contributes 0 triangles; cycle {20,21,22} contributes 1;
        // self-loop on 30 contributes 0.
        assert_eq!(g.triangle_count(), 1);

        // Two triangles sharing edge 1–2: {1,2,3} and {1,2,4}.
        // Directed edges: 1→2, 2→3, 3→1, 2→4, 4→1.
        let mut s2 = TripleStore::new();
        let p = 1u32;
        s2.insert(IdTriple::new(1, p, 2));
        s2.insert(IdTriple::new(2, p, 3));
        s2.insert(IdTriple::new(3, p, 1));
        s2.insert(IdTriple::new(2, p, 4));
        s2.insert(IdTriple::new(4, p, 1));
        let g2 = GraphView::from_store(&s2);
        assert_eq!(g2.triangle_count(), 2);
    }
}
