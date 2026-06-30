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
//! Implemented metrics: degree, BFS / shortest path, weakly- and
//! strongly-connected components (iterative Tarjan), PageRank, triangle count,
//! betweenness (Brandes) and closeness centrality, Louvain community detection,
//! and k-core decomposition.
//!
//! For predicate-labeled / weighted analysis, [`WeightedGraphView`] keeps a
//! predicate id and a numeric weight on every edge and adds weighted
//! single-source shortest paths (Dijkstra), predicate-filtered traversal,
//! weighted PageRank, and a branch-and-bound top-k subgraph query (mirroring
//! gStore's `topk` module).  The unweighted [`GraphView`] API is unchanged.
//!
//! # Deferred work
//! * Directed-modularity Louvain (current implementation symmetrises edges)

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};

use crate::model::id::{EntityLiteralId, PredId};
use crate::model::IdTriple;
use crate::store::{TripleSource, TripleStore};

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
        Self::from_source(store)
    }

    /// Build a [`GraphView`] from any [`TripleSource`] (in-memory or on-disk),
    /// so analytics-backed SPARQL built-ins can run over whatever store the
    /// query engine currently holds.
    pub fn from_source<S: TripleSource>(store: &S) -> Self {
        // Collect raw (sub, obj) pairs from every triple in one pass. The trait
        // `iter_all` returns an owned Vec (vs `TripleStore`'s inherent iterator).
        let raw_pairs: Vec<(EntityLiteralId, EntityLiteralId)> =
            store.iter_all().into_iter().map(|t| (t.sub, t.obj)).collect();
        Self::from_raw_pairs(raw_pairs)
    }

    /// Assemble the CSR adjacency from raw `(subject, object)` pairs.
    fn from_raw_pairs(raw_pairs: Vec<(EntityLiteralId, EntityLiteralId)>) -> Self {
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

    // ---- thin wrappers backing the gStore graph SPARQL built-ins ----------

    /// Length (in edges) of the shortest directed path `src ⇝ dst`, backing the
    /// `SHORTESTPATHLEN` built-in. `Some(0)` when `src == dst`; `None` when
    /// either node is absent or no directed path exists.
    pub fn shortest_path_len(
        &self,
        src: EntityLiteralId,
        dst: EntityLiteralId,
    ) -> Option<usize> {
        self.shortest_path(src, dst).map(|p| p.len() - 1)
    }

    /// Whether `dst` is reachable from `src` within `max_hops` directed edges,
    /// backing the `KHOPREACHABLE` built-in. A node always reaches itself
    /// (0 hops). `None` when either node is absent.
    pub fn khop_reachable(
        &self,
        src: EntityLiteralId,
        dst: EntityLiteralId,
        max_hops: u32,
    ) -> Option<bool> {
        let src_i = *self.id_to_idx.get(&src)?;
        let dst_i = *self.id_to_idx.get(&dst)?;
        if src_i == dst_i {
            return Some(true);
        }
        let n = self.nodes.len();
        let mut dist = vec![u32::MAX; n];
        dist[src_i] = 0;
        let mut queue = VecDeque::new();
        queue.push_back(src_i);
        while let Some(u) = queue.pop_front() {
            let d = dist[u];
            if d >= max_hops {
                continue; // budget exhausted — do not expand further
            }
            for &v in &self.out_adj[self.out_ptr[u]..self.out_ptr[u + 1]] {
                if dist[v] == u32::MAX {
                    if v == dst_i {
                        return Some(true);
                    }
                    dist[v] = d + 1;
                    queue.push_back(v);
                }
            }
        }
        Some(false)
    }

    /// Whether the directed graph contains any cycle, backing the
    /// `CYCLEBOOLEAN` / `SIMPLECYCLEBOOLEAN` built-ins. True iff some strongly
    /// connected component spans more than one node, or any node has a
    /// self-loop edge (`x → x`).
    pub fn has_cycle(&self) -> bool {
        // A self-loop is a one-node cycle that Tarjan leaves as a singleton SCC.
        for (i, _) in self.nodes.iter().enumerate() {
            if self.out_adj[self.out_ptr[i]..self.out_ptr[i + 1]].contains(&i) {
                return true;
            }
        }
        let (comp_map, num_comp) = self.strongly_connected_components();
        let mut sizes = vec![0usize; num_comp];
        for &c in comp_map.values() {
            sizes[c] += 1;
        }
        sizes.iter().any(|&s| s > 1)
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

    // ---- Strongly-connected components (iterative Tarjan) ------------------

    /// Strongly-connected components via Tarjan's algorithm, implemented
    /// **iteratively** with an explicit work stack (no recursion — safe for
    /// deep graphs that would overflow the call stack).
    ///
    /// Two nodes are in the same SCC iff each is reachable from the other along
    /// directed edges.  Returns `(comp_map, component_count)` where `comp_map`
    /// maps every entity id to a 0-based component id.  Component ids are
    /// assigned in the order SCCs are finalised (reverse-topological order of
    /// the condensation), but callers should treat the concrete ids as opaque.
    ///
    /// A directed cycle collapses to a single SCC; an acyclic chain of `N`
    /// nodes yields `N` singleton SCCs.  Self-loops do not merge a node with
    /// any other, so a self-looping node remains a singleton SCC.
    pub fn strongly_connected_components(
        &self,
    ) -> (HashMap<EntityLiteralId, usize>, usize) {
        let n = self.nodes.len();
        const UNVISITED: usize = usize::MAX;

        let mut index = vec![UNVISITED; n]; // discovery order, MAX = unvisited
        let mut lowlink = vec![0usize; n];
        let mut on_stack = vec![false; n];
        let mut comp_id = vec![UNVISITED; n];
        let mut tarjan_stack: Vec<usize> = Vec::new();

        let mut next_index = 0usize;
        let mut num_comp = 0usize;

        // Explicit DFS work stack: each frame is `(node, next-edge cursor into
        // out_adj)`.  `(usize, usize)` is `Copy`, so peeking the top frame never
        // borrows the stack across the `push` that descends into a child.
        let mut work: Vec<(usize, usize)> = Vec::new();

        for start in 0..n {
            if index[start] != UNVISITED {
                continue;
            }
            // Open `start`.
            index[start] = next_index;
            lowlink[start] = next_index;
            next_index += 1;
            tarjan_stack.push(start);
            on_stack[start] = true;
            work.push((start, self.out_ptr[start]));

            while let Some(&(v, cursor)) = work.last() {
                if cursor < self.out_ptr[v + 1] {
                    let w = self.out_adj[cursor];
                    work.last_mut().unwrap().1 = cursor + 1; // advance edge cursor
                    if index[w] == UNVISITED {
                        // Tree edge: descend into `w`.
                        index[w] = next_index;
                        lowlink[w] = next_index;
                        next_index += 1;
                        tarjan_stack.push(w);
                        on_stack[w] = true;
                        work.push((w, self.out_ptr[w]));
                    } else if on_stack[w] {
                        // Back/forward edge to a node still on the Tarjan stack.
                        lowlink[v] = lowlink[v].min(index[w]);
                    }
                    // Edge into an already-finalised SCC: ignored.
                } else {
                    // All edges of `v` explored: `v` is an SCC root iff
                    // lowlink == index.
                    if lowlink[v] == index[v] {
                        loop {
                            let w = tarjan_stack.pop().unwrap();
                            on_stack[w] = false;
                            comp_id[w] = num_comp;
                            if w == v {
                                break;
                            }
                        }
                        num_comp += 1;
                    }
                    work.pop();
                    if let Some(&(parent, _)) = work.last() {
                        lowlink[parent] = lowlink[parent].min(lowlink[v]);
                    }
                }
            }
        }

        let comp_map = self
            .nodes
            .iter()
            .enumerate()
            .map(|(i, &id)| (id, comp_id[i]))
            .collect();
        (comp_map, num_comp)
    }

    // ---- Betweenness centrality (Brandes) ---------------------------------

    /// Betweenness centrality for every node via Brandes' algorithm over the
    /// unweighted directed graph.
    ///
    /// For each ordered pair of distinct nodes `(s, t)`, a node `v` accrues the
    /// fraction of shortest `s ⇝ t` paths that pass through `v` (excluding the
    /// endpoints).  Scores are **not** divided by two: this is a directed graph,
    /// so each ordered pair contributes once.
    ///
    /// Complexity is `O(V · (V + E))` time and `O(V + E)` extra space.
    pub fn betweenness_centrality(&self) -> HashMap<EntityLiteralId, f64> {
        let n = self.nodes.len();
        let mut bc = vec![0.0f64; n];

        for s in 0..n {
            // --- single-source shortest-path counting (BFS) ---
            let mut stack: Vec<usize> = Vec::new(); // nodes in non-decreasing dist
            let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
            let mut sigma = vec![0.0f64; n]; // # shortest paths s ⇝ v
            let mut dist = vec![-1i64; n];
            sigma[s] = 1.0;
            dist[s] = 0;

            let mut queue = VecDeque::new();
            queue.push_back(s);
            while let Some(v) = queue.pop_front() {
                stack.push(v);
                for &w in &self.out_adj[self.out_ptr[v]..self.out_ptr[v + 1]] {
                    if dist[w] < 0 {
                        dist[w] = dist[v] + 1;
                        queue.push_back(w);
                    }
                    if dist[w] == dist[v] + 1 {
                        sigma[w] += sigma[v];
                        preds[w].push(v);
                    }
                }
            }

            // --- back-propagation of dependencies ---
            let mut delta = vec![0.0f64; n];
            while let Some(w) = stack.pop() {
                let coeff = (1.0 + delta[w]) / sigma[w];
                for &v in &preds[w] {
                    delta[v] += sigma[v] * coeff;
                }
                if w != s {
                    bc[w] += delta[w];
                }
            }
        }

        self.nodes.iter().enumerate().map(|(i, &id)| (id, bc[i])).collect()
    }

    // ---- Closeness centrality ---------------------------------------------

    /// Closeness centrality for every node, using the Wasserman–Faust
    /// normalisation so that the metric remains meaningful on disconnected
    /// graphs.
    ///
    /// Distances are measured along **out-edges** (how close a node is to the
    /// set it can reach).  Let `r` be the number of nodes reachable from `v`
    /// (including `v`) and `S` the sum of those forward distances.  The score is
    ///
    /// ```text
    /// C(v) = ((r - 1) / S) · ((r - 1) / (n - 1))
    /// ```
    ///
    /// where the first factor is the inverse mean distance to the reachable set
    /// and the second scales it by the fraction of the graph that is reachable.
    /// A node that reaches nothing (and a single-node graph) scores `0`.
    pub fn closeness_centrality(&self) -> HashMap<EntityLiteralId, f64> {
        let n = self.nodes.len();
        let mut result = HashMap::with_capacity(n);
        if n <= 1 {
            for &id in &self.nodes {
                result.insert(id, 0.0);
            }
            return result;
        }

        let mut dist = vec![-1i64; n];
        let mut queue = VecDeque::new();
        for s in 0..n {
            // BFS from `s` over out-edges.
            dist[s] = 0;
            queue.push_back(s);
            let mut sum_dist: i64 = 0;
            let mut reach: i64 = 0; // reachable nodes excluding `s`
            while let Some(v) = queue.pop_front() {
                for &w in &self.out_adj[self.out_ptr[v]..self.out_ptr[v + 1]] {
                    if dist[w] < 0 {
                        dist[w] = dist[v] + 1;
                        sum_dist += dist[w];
                        reach += 1;
                        queue.push_back(w);
                    }
                }
            }

            let score = if sum_dist > 0 {
                let r1 = reach as f64; // (r - 1): reachable excluding self
                (r1 / sum_dist as f64) * (r1 / (n as f64 - 1.0))
            } else {
                0.0
            };
            result.insert(self.nodes[s], score);

            // Reset distances for the next source's BFS.
            for d in dist.iter_mut() {
                *d = -1;
            }
        }
        result
    }

    // ---- Louvain community detection --------------------------------------

    /// Community detection via the Louvain method (multi-level modularity
    /// maximisation).
    ///
    /// The directed graph is symmetrised into an undirected weighted graph:
    /// the weight between two distinct nodes is the number of directed edges
    /// joining them (1 or 2), and a directed self-loop contributes a unit
    /// self-loop weight.  The algorithm then alternates a local-moving phase
    /// (greedily move each node into the neighbouring community giving the
    /// largest positive modularity gain) with an aggregation phase (collapse
    /// each community into a super-node), repeating until no node moves.
    ///
    /// The modularity gain of moving an isolated node `i` into community `C` is
    ///
    /// ```text
    /// ΔQ = k_{i,in}(C) / m − (Σ_tot(C) · k_i) / (2 m²)
    /// ```
    ///
    /// where `m` is the total edge weight, `k_i` the weighted degree of `i`
    /// (self-loops counted twice), `Σ_tot(C)` the sum of degrees in `C`, and
    /// `k_{i,in}(C)` the weight of edges from `i` into `C`.
    ///
    /// Returns `entity_id → community id` (0-based, opaque).
    pub fn louvain(&self) -> HashMap<EntityLiteralId, usize> {
        let n = self.nodes.len();
        if n == 0 {
            return HashMap::new();
        }

        // --- build the initial undirected weighted graph (dense indices) ---
        let mut edge_w: HashMap<(usize, usize), f64> = HashMap::new();
        let mut self_w = vec![0.0f64; n];
        for (u, span) in self.out_ptr.windows(2).enumerate() {
            for &v in &self.out_adj[span[0]..span[1]] {
                if u == v {
                    self_w[u] += 1.0;
                } else {
                    *edge_w.entry((u.min(v), u.max(v))).or_insert(0.0) += 1.0;
                }
            }
        }

        // Total edge weight `m` (each undirected edge once + self-loops).
        let m: f64 = edge_w.values().sum::<f64>() + self_w.iter().sum::<f64>();
        if m == 0.0 {
            // No edges: every node is its own community.
            return self
                .nodes
                .iter()
                .enumerate()
                .map(|(i, &id)| (id, i))
                .collect();
        }

        // Symmetric adjacency for the working graph.
        let mut adj: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
        for (&(a, b), &w) in &edge_w {
            adj[a].push((b, w));
            adj[b].push((a, w));
        }
        let mut self_loops = self_w;
        let mut deg: Vec<f64> = (0..n)
            .map(|i| adj[i].iter().map(|&(_, w)| w).sum::<f64>() + 2.0 * self_loops[i])
            .collect();

        // Mapping from each original dense index to its current super-node.
        let mut orig_to_super: Vec<usize> = (0..n).collect();
        let mut cur_n = n;

        loop {
            let comm = Self::louvain_one_level(&adj, &deg, m);
            let (renumbered, k) = Self::renumber(&comm);

            // Fold this level's assignment into the original→super mapping.
            for s in orig_to_super.iter_mut() {
                *s = renumbered[*s];
            }

            if k == cur_n {
                break; // no community merged: converged
            }

            // --- aggregate communities into super-nodes ---
            let mut new_self = vec![0.0f64; k];
            let mut new_edge: HashMap<(usize, usize), f64> = HashMap::new();
            for u in 0..cur_n {
                let cu = renumbered[u];
                new_self[cu] += self_loops[u];
                for &(v, w) in &adj[u] {
                    if v < u {
                        continue; // visit each undirected edge once (u ≤ v)
                    }
                    let cv = renumbered[v];
                    if cu == cv {
                        new_self[cu] += w; // internal edge → self-loop weight w
                    } else {
                        *new_edge.entry((cu, cv)).or_insert(0.0) += w;
                        *new_edge.entry((cv, cu)).or_insert(0.0) += w;
                    }
                }
            }

            let mut next_adj: Vec<Vec<(usize, f64)>> = vec![Vec::new(); k];
            for (&(a, b), &w) in &new_edge {
                next_adj[a].push((b, w));
            }
            let next_deg: Vec<f64> = (0..k)
                .map(|i| {
                    next_adj[i].iter().map(|&(_, w)| w).sum::<f64>() + 2.0 * new_self[i]
                })
                .collect();

            adj = next_adj;
            self_loops = new_self;
            deg = next_deg;
            cur_n = k;
        }

        self.nodes
            .iter()
            .enumerate()
            .map(|(i, &id)| (id, orig_to_super[i]))
            .collect()
    }

    /// One Louvain local-moving pass: repeatedly sweep all nodes, moving each
    /// into the neighbouring community of maximum modularity gain, until a full
    /// sweep makes no move.  Returns the community label per node.
    fn louvain_one_level(adj: &[Vec<(usize, f64)>], deg: &[f64], m: f64) -> Vec<usize> {
        let n = adj.len();
        let mut comm: Vec<usize> = (0..n).collect();
        let mut sigma_tot: Vec<f64> = deg.to_vec(); // Σ degrees per community
        let two_m_sq = 2.0 * m * m;

        loop {
            let mut moved = false;
            for i in 0..n {
                let ci = comm[i];
                // Tentatively remove `i` from its community.
                sigma_tot[ci] -= deg[i];

                // Weight from `i` to each neighbouring community.
                let mut k_in: HashMap<usize, f64> = HashMap::new();
                for &(j, w) in &adj[i] {
                    if j != i {
                        *k_in.entry(comm[j]).or_insert(0.0) += w;
                    }
                }

                // Baseline gain: re-joining the original community.
                let base = k_in.get(&ci).copied().unwrap_or(0.0);
                let mut best_comm = ci;
                let mut best_gain = base / m - sigma_tot[ci] * deg[i] / two_m_sq;

                for (&c, &w) in &k_in {
                    let gain = w / m - sigma_tot[c] * deg[i] / two_m_sq;
                    if gain > best_gain + 1e-12 {
                        best_gain = gain;
                        best_comm = c;
                    }
                }

                sigma_tot[best_comm] += deg[i];
                comm[i] = best_comm;
                if best_comm != ci {
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
        comm
    }

    /// Compact community labels to a contiguous `0..k` range.
    /// Returns `(relabelled, k)`.
    fn renumber(comm: &[usize]) -> (Vec<usize>, usize) {
        let mut map: HashMap<usize, usize> = HashMap::new();
        let mut out = vec![0usize; comm.len()];
        let mut k = 0usize;
        for (i, &c) in comm.iter().enumerate() {
            let label = *map.entry(c).or_insert_with(|| {
                let cur = k;
                k += 1;
                cur
            });
            out[i] = label;
        }
        (out, k)
    }

    // ---- k-core decomposition ---------------------------------------------

    /// k-core decomposition: the core number of each node, computed by the
    /// linear-time Batagelj–Zaversnik peeling algorithm over the undirected
    /// view (directions ignored, self-loops and parallel edges removed).
    ///
    /// A node's core number is the largest `k` such that it belongs to a maximal
    /// subgraph in which every vertex has degree ≥ `k`.  Equivalently, it is the
    /// degree the node still has at the moment it is removed when vertices are
    /// repeatedly peeled in non-decreasing residual-degree order.
    ///
    /// Returns `entity_id → core number`.
    pub fn k_core(&self) -> HashMap<EntityLiteralId, usize> {
        let n = self.nodes.len();
        if n == 0 {
            return HashMap::new();
        }

        // Undirected adjacency (no self-loops, de-duplicated).
        let mut udj: Vec<Vec<usize>> = vec![Vec::new(); n];
        for u in 0..n {
            for &v in &self.out_adj[self.out_ptr[u]..self.out_ptr[u + 1]] {
                if u != v {
                    udj[u].push(v);
                    udj[v].push(u);
                }
            }
        }
        for a in udj.iter_mut() {
            a.sort_unstable();
            a.dedup();
        }

        let mut deg: Vec<usize> = udj.iter().map(Vec::len).collect();
        let max_deg = deg.iter().copied().max().unwrap_or(0);

        // Bin-sort vertices by current degree (`bin[d]` = first slot of degree d).
        let mut bin = vec![0usize; max_deg + 2];
        for &d in &deg {
            bin[d] += 1;
        }
        let mut start = 0usize;
        for slot in bin.iter_mut().take(max_deg + 1) {
            let count = *slot;
            *slot = start;
            start += count;
        }

        let mut vert = vec![0usize; n]; // vertices ordered by degree
        let mut pos = vec![0usize; n]; // pos[v] = index of v within `vert`
        {
            let mut fill = bin.clone();
            for v in 0..n {
                let d = deg[v];
                pos[v] = fill[d];
                vert[fill[d]] = v;
                fill[d] += 1;
            }
        }

        // Peel vertices in increasing residual degree; the degree a vertex has
        // when reached is its core number.
        let mut core = vec![0usize; n];
        for i in 0..n {
            let v = vert[i];
            core[v] = deg[v];
            let neighbors = std::mem::take(&mut udj[v]);
            for &u in &neighbors {
                if deg[u] > deg[v] {
                    let du = deg[u];
                    let pu = pos[u];
                    let pw = bin[du]; // first slot of u's current degree bucket
                    let w = vert[pw];
                    if u != w {
                        // Swap `u` to the front of its bucket.
                        vert[pu] = w;
                        vert[pw] = u;
                        pos[w] = pu;
                        pos[u] = pw;
                    }
                    bin[du] += 1; // shrink the degree-`du` bucket from the left
                    deg[u] -= 1; // `u` drops one degree
                }
            }
            udj[v] = neighbors;
        }

        self.nodes.iter().enumerate().map(|(i, &id)| (id, core[i])).collect()
    }
}

// ===========================================================================
// WeightedGraphView — predicate-labeled, weighted edges
// ===========================================================================

/// Lookup of the parallel data edges between an ordered dense pair, used by the
/// top-k matcher: `(src_dense, dst_dense) → [(predicate, weight), …]`.
type PairEdges = HashMap<(usize, usize), Vec<(PredId, f64)>>;

/// A single labeled, weighted edge stored in [`WeightedGraphView`]'s CSR arrays.
///
/// For an out-edge `node` is the target; for an in-edge `node` is the source.
#[derive(Copy, Clone, Debug)]
struct WEdge {
    node: usize,
    pred: PredId,
    weight: f64,
}

/// Min-heap entry for Dijkstra.
///
/// `BinaryHeap` is a max-heap, so [`Ord`] is inverted to pop the *smallest*
/// tentative distance first.  Ties break on the smaller dense node index, which
/// (because dense indices follow sorted entity ids) yields stable, id-ordered
/// results.
#[derive(Copy, Clone)]
struct DijkstraItem {
    dist: f64,
    node: usize,
}

impl PartialEq for DijkstraItem {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for DijkstraItem {}
impl Ord for DijkstraItem {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .dist
            .total_cmp(&self.dist)
            .then_with(|| other.node.cmp(&self.node))
    }
}
impl PartialOrd for DijkstraItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Total-order wrapper over `f64` scores so they can drive a `BinaryHeap` for
/// the top-k threshold (NaN is not produced by the supported scoring paths).
#[derive(Copy, Clone)]
struct OrdF(f64);
impl PartialEq for OrdF {
    fn eq(&self, other: &Self) -> bool {
        self.0.total_cmp(&other.0) == Ordering::Equal
    }
}
impl Eq for OrdF {}
impl Ord for OrdF {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}
impl PartialOrd for OrdF {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

// ---- top-k query types ----------------------------------------------------

/// One edge of a top-k query pattern.
///
/// `src` and `dst` index the pattern's variables in `[0, num_vars)`.  `pred`
/// constrains the data edge: `None` matches any predicate, `Some(p)` matches
/// only predicate `p`.
#[derive(Copy, Clone, Debug)]
pub struct PatternEdge {
    pub src: usize,
    pub dst: usize,
    pub pred: Option<PredId>,
}

impl PatternEdge {
    /// Edge constrained to a specific predicate.
    pub fn labeled(src: usize, dst: usize, pred: PredId) -> PatternEdge {
        PatternEdge { src, dst, pred: Some(pred) }
    }

    /// Edge that matches any predicate.
    pub fn any(src: usize, dst: usize) -> PatternEdge {
        PatternEdge { src, dst, pred: None }
    }
}

/// A connected top-k query pattern over `num_vars` variables.
#[derive(Clone, Debug)]
pub struct QueryPattern {
    pub num_vars: usize,
    pub edges: Vec<PatternEdge>,
}

impl QueryPattern {
    /// Build a pattern from a variable count and its edges.
    pub fn new(num_vars: usize, edges: Vec<PatternEdge>) -> QueryPattern {
        QueryPattern { num_vars, edges }
    }
}

/// A data edge bound to a pattern edge in a [`SubgraphMatch`].
#[derive(Copy, Clone, Debug)]
pub struct MatchedEdge {
    pub src: EntityLiteralId,
    pub pred: PredId,
    pub dst: EntityLiteralId,
    pub weight: f64,
}

/// A single subgraph match returned by the top-k query.
#[derive(Clone, Debug)]
pub struct SubgraphMatch {
    /// `vars[i]` is the entity id bound to pattern variable `i`.
    pub vars: Vec<EntityLiteralId>,
    /// Matched data edges, in the same order as the pattern's edges.
    pub edges: Vec<MatchedEdge>,
    /// Score assigned to this match by the scoring function.
    pub score: f64,
}

/// Read-only context shared across the top-k backtracking recursion.
struct TopkCtx<'a> {
    pattern: &'a QueryPattern,
    /// Variables in connectivity-driven matching order.
    order: &'a [usize],
    /// `order_pos[var]` = position of `var` within `order`.
    order_pos: &'a [usize],
    /// Pattern-edge indices that become fully bound at each order position.
    closed_at: &'a [Vec<usize>],
    pair: &'a PairEdges,
    /// Per-pattern-edge global maximum weight (admissible branch-and-bound bound).
    max_w: &'a [f64],
    k: usize,
    prune: bool,
    score: &'a dyn Fn(&[EntityLiteralId], &[MatchedEdge]) -> f64,
}

/// Mutable state threaded through the top-k backtracking recursion.
struct TopkState {
    /// `assign[var]` = bound dense index, or `usize::MAX` if unbound.
    assign: Vec<usize>,
    /// Injectivity guard: `used[dense]` is true while a variable holds it.
    used: Vec<bool>,
    results: Vec<SubgraphMatch>,
    /// Min-heap of the best `k` scores seen so far (drives bound pruning).
    topk: BinaryHeap<Reverse<OrdF>>,
    /// Sum of weights of already fully-bound pattern edges.
    partial: f64,
    /// Sum of `max_w` over not-yet-bound pattern edges.
    remaining: f64,
}

/// A directed graph view whose edges carry a predicate id and a numeric weight.
///
/// This is a *parallel* view to [`GraphView`]: it is built independently and
/// leaves the unweighted API untouched.  Unlike `GraphView`, edges are **not**
/// de-duplicated across predicates — `(s, p₁, o)` and `(s, p₂, o)` are kept as
/// two distinct labeled edges — so predicate-aware traversal is exact.  Within
/// each node the CSR adjacency is sorted by `(neighbour, predicate)` for
/// deterministic iteration.
///
/// Construction is O(E log E).  Dijkstra is O(E log V); weighted PageRank is
/// O(V + E) per iteration; top-k complexity is documented on the query methods.
pub struct WeightedGraphView {
    nodes: Vec<EntityLiteralId>,
    id_to_idx: HashMap<EntityLiteralId, usize>,
    out_ptr: Vec<usize>,
    out_edges: Vec<WEdge>,
    in_ptr: Vec<usize>,
    in_edges: Vec<WEdge>,
}

impl WeightedGraphView {
    /// Build a weighted view, deriving each edge's weight from its triple.
    ///
    /// Every triple `(s, p, o)` becomes a labeled edge `s → o` with predicate
    /// `p` and weight `weight_of(&triple)`.  Identical triples are de-duplicated;
    /// triples that differ only in predicate stay as separate edges.
    pub fn from_store_with_weights<F>(store: &TripleStore, weight_of: F) -> Self
    where
        F: Fn(&IdTriple) -> f64,
    {
        // 1. Raw labeled edges (entity-id space).
        let raw: Vec<(EntityLiteralId, EntityLiteralId, PredId, f64)> = store
            .iter_all()
            .map(|t| (t.sub, t.obj, t.pred, weight_of(&t)))
            .collect();

        // 2. Sorted, de-duplicated node list + sparse→dense lookup.
        let mut id_set: Vec<EntityLiteralId> =
            raw.iter().flat_map(|&(s, o, _, _)| [s, o]).collect();
        id_set.sort_unstable();
        id_set.dedup();
        let n = id_set.len();
        let id_to_idx: HashMap<EntityLiteralId, usize> =
            id_set.iter().copied().enumerate().map(|(i, id)| (id, i)).collect();

        // 3. Dense edges, sorted by (s, o, pred) and de-duplicated by (s, p, o).
        let mut edges: Vec<(usize, usize, PredId, f64)> = raw
            .iter()
            .map(|&(s, o, p, w)| (id_to_idx[&s], id_to_idx[&o], p, w))
            .collect();
        edges.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        edges.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1 && a.2 == b.2);
        let m = edges.len();

        // 4. Out-CSR (edges already in (s, o, pred) order).
        let mut out_deg = vec![0usize; n];
        for &(s, _, _, _) in &edges {
            out_deg[s] += 1;
        }
        let mut out_ptr = vec![0usize; n + 1];
        for i in 0..n {
            out_ptr[i + 1] = out_ptr[i] + out_deg[i];
        }
        let mut out_edges = vec![WEdge { node: 0, pred: 0, weight: 0.0 }; m];
        {
            let mut cur = out_ptr[..n].to_vec();
            for &(s, o, p, w) in &edges {
                out_edges[cur[s]] = WEdge { node: o, pred: p, weight: w };
                cur[s] += 1;
            }
        }

        // 5. In-CSR (re-sort by (o, s, pred) so each bucket is deterministic).
        let mut iedges = edges;
        iedges.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)).then(a.2.cmp(&b.2)));
        let mut in_deg = vec![0usize; n];
        for &(_, o, _, _) in &iedges {
            in_deg[o] += 1;
        }
        let mut in_ptr = vec![0usize; n + 1];
        for i in 0..n {
            in_ptr[i + 1] = in_ptr[i] + in_deg[i];
        }
        let mut in_edges = vec![WEdge { node: 0, pred: 0, weight: 0.0 }; m];
        {
            let mut cur = in_ptr[..n].to_vec();
            for &(s, o, p, w) in &iedges {
                in_edges[cur[o]] = WEdge { node: s, pred: p, weight: w };
                cur[o] += 1;
            }
        }

        WeightedGraphView { nodes: id_set, id_to_idx, out_ptr, out_edges, in_ptr, in_edges }
    }

    /// Build a weighted view with unit weight (1.0) on every edge, retaining the
    /// predicate label.  Useful for predicate-filtered traversal.
    pub fn from_store(store: &TripleStore) -> Self {
        Self::from_store_with_weights(store, |_| 1.0)
    }

    /// Number of nodes in the graph.
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of labeled edges (parallel predicates counted separately).
    pub fn edge_count(&self) -> usize {
        self.out_edges.len()
    }

    // ---- weighted single-source shortest path (Dijkstra) ------------------

    /// Core Dijkstra over non-negative edge weights from dense source `src_i`.
    ///
    /// When `pred_filter` is `Some(set)`, only edges whose predicate is in
    /// `set` are relaxed.  Returns `(dist, prev)` keyed by dense index; `dist`
    /// is `INFINITY` for unreachable nodes and `prev` is `usize::MAX` for nodes
    /// without a predecessor.
    fn dijkstra(&self, src_i: usize, pred_filter: Option<&HashSet<PredId>>) -> (Vec<f64>, Vec<usize>) {
        let n = self.nodes.len();
        let mut dist = vec![f64::INFINITY; n];
        let mut prev = vec![usize::MAX; n];
        let mut heap: BinaryHeap<DijkstraItem> = BinaryHeap::new();
        dist[src_i] = 0.0;
        heap.push(DijkstraItem { dist: 0.0, node: src_i });

        while let Some(DijkstraItem { dist: d, node: u }) = heap.pop() {
            if d > dist[u] {
                continue; // stale heap entry
            }
            for e in &self.out_edges[self.out_ptr[u]..self.out_ptr[u + 1]] {
                if let Some(set) = pred_filter {
                    if !set.contains(&e.pred) {
                        continue;
                    }
                }
                let nd = d + e.weight;
                if nd < dist[e.node] {
                    dist[e.node] = nd;
                    prev[e.node] = u;
                    heap.push(DijkstraItem { dist: nd, node: e.node });
                }
            }
        }
        (dist, prev)
    }

    /// Collect finite dense distances into an entity-id-keyed map.
    fn collect_dist(&self, dist: &[f64]) -> HashMap<EntityLiteralId, f64> {
        self.nodes
            .iter()
            .enumerate()
            .filter(|&(i, _)| dist[i].is_finite())
            .map(|(i, &id)| (id, dist[i]))
            .collect()
    }

    /// Weighted single-source shortest-path distances from `src` over
    /// non-negative edge weights.  Returns `entity_id → distance` for every
    /// reachable node (including `src` at 0.0), or `None` if `src` is absent.
    pub fn weighted_distances(&self, src: EntityLiteralId) -> Option<HashMap<EntityLiteralId, f64>> {
        let s = *self.id_to_idx.get(&src)?;
        let (dist, _) = self.dijkstra(s, None);
        Some(self.collect_dist(&dist))
    }

    /// As [`weighted_distances`](Self::weighted_distances), restricted to edges
    /// whose predicate is in `preds`.
    pub fn weighted_distances_filtered(
        &self,
        src: EntityLiteralId,
        preds: &HashSet<PredId>,
    ) -> Option<HashMap<EntityLiteralId, f64>> {
        let s = *self.id_to_idx.get(&src)?;
        let (dist, _) = self.dijkstra(s, Some(preds));
        Some(self.collect_dist(&dist))
    }

    fn weighted_path_impl(
        &self,
        src: EntityLiteralId,
        dst: EntityLiteralId,
        pred_filter: Option<&HashSet<PredId>>,
    ) -> Option<(f64, Vec<EntityLiteralId>)> {
        let s = *self.id_to_idx.get(&src)?;
        let t = *self.id_to_idx.get(&dst)?;
        if s == t {
            return Some((0.0, vec![src]));
        }
        let (dist, prev) = self.dijkstra(s, pred_filter);
        if !dist[t].is_finite() {
            return None;
        }
        let mut path = Vec::new();
        let mut cur = t;
        while cur != s {
            path.push(self.nodes[cur]);
            cur = prev[cur];
        }
        path.push(self.nodes[s]);
        path.reverse();
        Some((dist[t], path))
    }

    /// Minimum-weight directed path from `src` to `dst` over non-negative edge
    /// weights.  Returns `(total_weight, path)` with `path` inclusive of both
    /// endpoints, or `None` if either node is absent or `dst` is unreachable.
    /// Returns `(0.0, [src])` when `src == dst`.
    pub fn weighted_shortest_path(
        &self,
        src: EntityLiteralId,
        dst: EntityLiteralId,
    ) -> Option<(f64, Vec<EntityLiteralId>)> {
        self.weighted_path_impl(src, dst, None)
    }

    /// As [`weighted_shortest_path`](Self::weighted_shortest_path), restricted to
    /// edges whose predicate is in `preds`.
    pub fn weighted_shortest_path_filtered(
        &self,
        src: EntityLiteralId,
        dst: EntityLiteralId,
        preds: &HashSet<PredId>,
    ) -> Option<(f64, Vec<EntityLiteralId>)> {
        self.weighted_path_impl(src, dst, Some(preds))
    }

    // ---- predicate-filtered BFS -------------------------------------------

    /// Single-source BFS hop distances from `src`, traversing only edges whose
    /// predicate is in `preds`.  Returns `entity_id → hop count`, or `None` if
    /// `src` is absent.
    pub fn bfs_distances_filtered(
        &self,
        src: EntityLiteralId,
        preds: &HashSet<PredId>,
    ) -> Option<HashMap<EntityLiteralId, u32>> {
        let s = *self.id_to_idx.get(&src)?;
        let n = self.nodes.len();
        let mut dist = vec![u32::MAX; n];
        dist[s] = 0;
        let mut queue = VecDeque::new();
        queue.push_back(s);
        while let Some(u) = queue.pop_front() {
            let d = dist[u];
            for e in &self.out_edges[self.out_ptr[u]..self.out_ptr[u + 1]] {
                if !preds.contains(&e.pred) {
                    continue;
                }
                if dist[e.node] == u32::MAX {
                    dist[e.node] = d + 1;
                    queue.push_back(e.node);
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

    // ---- weighted PageRank ------------------------------------------------

    /// Weighted PageRank: a node spreads its rank to out-neighbours in
    /// proportion to edge weight (rather than uniformly).
    ///
    /// `damping` is the usual factor (≈0.85); iteration stops once the L₁ change
    /// drops below `tol` or `max_iters` is reached.  Edge weights must be
    /// non-negative.  A node whose total out-weight is 0 is treated as dangling
    /// and redistributes its rank uniformly so that mass is conserved.
    ///
    /// Returns `entity_id → score`; scores sum to approximately 1.0.
    pub fn weighted_pagerank(
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
        let out_w: Vec<f64> = (0..n)
            .map(|i| {
                self.out_edges[self.out_ptr[i]..self.out_ptr[i + 1]]
                    .iter()
                    .map(|e| e.weight)
                    .sum()
            })
            .collect();

        for _ in 0..max_iters {
            let dangling_sum: f64 =
                (0..n).filter(|&i| out_w[i] <= 0.0).map(|i| rank[i]).sum();
            let base = (1.0 - damping + damping * dangling_sum) / n as f64;
            for v in next.iter_mut() {
                *v = base;
            }
            for u in 0..n {
                if out_w[u] > 0.0 {
                    let scale = damping * rank[u] / out_w[u];
                    for e in &self.out_edges[self.out_ptr[u]..self.out_ptr[u + 1]] {
                        next[e.node] += scale * e.weight;
                    }
                }
            }
            let delta: f64 =
                rank.iter().zip(next.iter()).map(|(a, b)| (a - b).abs()).sum();
            std::mem::swap(&mut rank, &mut next);
            if delta < tol {
                break;
            }
        }

        self.nodes.iter().enumerate().map(|(i, &id)| (id, rank[i])).collect()
    }

    // ---- top-k subgraph query ---------------------------------------------

    /// Map of every ordered dense pair to its parallel `(predicate, weight)`
    /// edges, used for closing-edge checks and representative selection.
    fn pair_edges(&self) -> PairEdges {
        let mut map: PairEdges = HashMap::new();
        for u in 0..self.nodes.len() {
            for e in &self.out_edges[self.out_ptr[u]..self.out_ptr[u + 1]] {
                map.entry((u, e.node)).or_default().push((e.pred, e.weight));
            }
        }
        map
    }

    /// Representative data edge between a fixed pair under a predicate
    /// constraint: the maximum-weight match, ties broken by the smallest
    /// predicate id.  `None` if no data edge satisfies the constraint.
    fn representative(list: Option<&[(PredId, f64)]>, pred: Option<PredId>) -> Option<(PredId, f64)> {
        let list = list?;
        let mut best: Option<(PredId, f64)> = None;
        for &(p, w) in list {
            if let Some(pc) = pred {
                if pc != p {
                    continue;
                }
            }
            best = match best {
                None => Some((p, w)),
                Some((bp, bw)) => match w.total_cmp(&bw) {
                    Ordering::Greater => Some((p, w)),
                    Ordering::Equal if p < bp => Some((p, w)),
                    _ => Some((bp, bw)),
                },
            };
        }
        best
    }

    /// Connectivity-driven variable order: a BFS over the pattern's variable
    /// graph so each variable after the first is adjacent to an earlier one
    /// (disconnected components are appended).  Returns `(order, order_pos)`.
    fn match_order(pattern: &QueryPattern) -> (Vec<usize>, Vec<usize>) {
        let nv = pattern.num_vars;
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); nv];
        for e in &pattern.edges {
            if e.src != e.dst {
                adj[e.src].push(e.dst);
                adj[e.dst].push(e.src);
            }
        }
        let mut visited = vec![false; nv];
        let mut order = Vec::with_capacity(nv);
        for start in 0..nv {
            if visited[start] {
                continue;
            }
            visited[start] = true;
            let mut queue = VecDeque::new();
            queue.push_back(start);
            while let Some(u) = queue.pop_front() {
                order.push(u);
                let mut nbrs = adj[u].clone();
                nbrs.sort_unstable();
                nbrs.dedup();
                for v in nbrs {
                    if !visited[v] {
                        visited[v] = true;
                        queue.push_back(v);
                    }
                }
            }
        }
        let mut order_pos = vec![0usize; nv];
        for (i, &v) in order.iter().enumerate() {
            order_pos[v] = i;
        }
        (order, order_pos)
    }

    /// Candidate dense nodes for the variable being bound at the current depth.
    ///
    /// A non-root variable is seeded from the adjacency of one already-bound
    /// neighbour (out- or in-edges, predicate-filtered), then filtered by its
    /// remaining constraints and injectivity.  A root variable (no constraint
    /// to an earlier variable) ranges over all unused nodes.  Candidates are
    /// returned in ascending dense order for deterministic enumeration.
    fn candidates(&self, ctx: &TopkCtx, st: &TopkState, var: usize) -> Vec<usize> {
        let n = self.nodes.len();
        let mypos = ctx.order_pos[var];
        // (fixed dense node, fixed_is_src, predicate) for each constraint to an
        // already-bound variable.
        let mut constraints: Vec<(usize, bool, Option<PredId>)> = Vec::new();
        for e in &ctx.pattern.edges {
            if e.src == e.dst {
                continue; // self-loops are verified when the edge closes
            }
            if e.src == var && ctx.order_pos[e.dst] < mypos {
                constraints.push((st.assign[e.dst], false, e.pred));
            } else if e.dst == var && ctx.order_pos[e.src] < mypos {
                constraints.push((st.assign[e.src], true, e.pred));
            }
        }

        if constraints.is_empty() {
            return (0..n).filter(|&c| !st.used[c]).collect();
        }

        let (fixed, fixed_is_src, pred0) = constraints[0];
        let mut base: Vec<usize> = if fixed_is_src {
            self.out_edges[self.out_ptr[fixed]..self.out_ptr[fixed + 1]]
                .iter()
                .filter(|e| pred0.is_none_or(|p| p == e.pred))
                .map(|e| e.node)
                .collect()
        } else {
            self.in_edges[self.in_ptr[fixed]..self.in_ptr[fixed + 1]]
                .iter()
                .filter(|e| pred0.is_none_or(|p| p == e.pred))
                .map(|e| e.node)
                .collect()
        };
        base.sort_unstable();
        base.dedup();
        base.into_iter()
            .filter(|&c| {
                if st.used[c] {
                    return false;
                }
                constraints[1..].iter().all(|&(fx, is_src, pred)| {
                    let (a, b) = if is_src { (fx, c) } else { (c, fx) };
                    Self::representative(ctx.pair.get(&(a, b)).map(Vec::as_slice), pred).is_some()
                })
            })
            .collect()
    }

    /// Backtracking core of the top-k matcher (see [`run_topk`](Self::run_topk)).
    fn topk_rec(&self, ctx: &TopkCtx, st: &mut TopkState, depth: usize) {
        if depth == ctx.order.len() {
            let vars: Vec<EntityLiteralId> =
                st.assign.iter().map(|&d| self.nodes[d]).collect();
            let mut edges = Vec::with_capacity(ctx.pattern.edges.len());
            for e in &ctx.pattern.edges {
                let a = st.assign[e.src];
                let b = st.assign[e.dst];
                let (p, w) = Self::representative(ctx.pair.get(&(a, b)).map(Vec::as_slice), e.pred)
                    .expect("closing checks guarantee every pattern edge is bound");
                edges.push(MatchedEdge { src: self.nodes[a], pred: p, dst: self.nodes[b], weight: w });
            }
            let s = (ctx.score)(&vars, &edges);
            if ctx.prune {
                if st.topk.len() < ctx.k {
                    st.topk.push(Reverse(OrdF(s)));
                } else if s > st.topk.peek().unwrap().0 .0 {
                    st.topk.pop();
                    st.topk.push(Reverse(OrdF(s)));
                }
            }
            st.results.push(SubgraphMatch { vars, edges, score: s });
            return;
        }

        // Branch-and-bound: prune when the best achievable score (bound on the
        // sum of edge weights) cannot beat the current k-th best.
        if ctx.prune && st.topk.len() == ctx.k {
            let threshold = st.topk.peek().unwrap().0 .0;
            if st.partial + st.remaining < threshold {
                return;
            }
        }

        let var = ctx.order[depth];
        for c in self.candidates(ctx, st, var) {
            st.assign[var] = c;
            st.used[c] = true;

            // Close every pattern edge whose later endpoint is this variable.
            let mut closed_ok = true;
            let mut add_partial = 0.0;
            let mut sub_remaining = 0.0;
            for &ei in &ctx.closed_at[depth] {
                let e = &ctx.pattern.edges[ei];
                let a = st.assign[e.src];
                let b = st.assign[e.dst];
                match Self::representative(ctx.pair.get(&(a, b)).map(Vec::as_slice), e.pred) {
                    Some((_, w)) => {
                        add_partial += w;
                        sub_remaining += ctx.max_w[ei];
                    }
                    None => {
                        closed_ok = false;
                        break;
                    }
                }
            }

            if closed_ok {
                st.partial += add_partial;
                st.remaining -= sub_remaining;
                self.topk_rec(ctx, st, depth + 1);
                st.partial -= add_partial;
                st.remaining += sub_remaining;
            }

            st.used[c] = false;
            st.assign[var] = usize::MAX;
        }
    }

    /// Shared top-k driver for both the default and custom-scored entry points.
    fn run_topk(
        &self,
        pattern: &QueryPattern,
        k: usize,
        prune: bool,
        score: &dyn Fn(&[EntityLiteralId], &[MatchedEdge]) -> f64,
    ) -> Vec<SubgraphMatch> {
        if k == 0 || pattern.num_vars == 0 {
            return Vec::new();
        }
        let pair = self.pair_edges();
        let (order, order_pos) = Self::match_order(pattern);

        let mut closed_at = vec![Vec::new(); pattern.num_vars];
        for (ei, e) in pattern.edges.iter().enumerate() {
            let pos = order_pos[e.src].max(order_pos[e.dst]);
            closed_at[pos].push(ei);
        }

        // Global max weight per pattern edge: an admissible bound on its
        // contribution to any match's score.
        let max_w: Vec<f64> = pattern
            .edges
            .iter()
            .map(|e| {
                let mx = self
                    .out_edges
                    .iter()
                    .filter(|we| e.pred.is_none_or(|p| p == we.pred))
                    .map(|we| we.weight)
                    .fold(f64::NEG_INFINITY, f64::max);
                if mx.is_finite() {
                    mx
                } else {
                    0.0
                }
            })
            .collect();
        let total_max: f64 = max_w.iter().sum();

        let ctx = TopkCtx {
            pattern,
            order: &order,
            order_pos: &order_pos,
            closed_at: &closed_at,
            pair: &pair,
            max_w: &max_w,
            k,
            prune,
            score,
        };
        let mut st = TopkState {
            assign: vec![usize::MAX; pattern.num_vars],
            used: vec![false; self.nodes.len()],
            results: Vec::new(),
            topk: BinaryHeap::new(),
            partial: 0.0,
            remaining: total_max,
        };
        self.topk_rec(&ctx, &mut st, 0);

        // Deterministic top-k: score descending, then assignment lexicographic.
        let mut results = st.results;
        results.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.vars.cmp(&b.vars)));
        results.truncate(k);
        results
    }

    /// Top-k subgraph matches scored by the **sum of matched edge weights**.
    ///
    /// Enumerates injective assignments of the pattern's variables to distinct
    /// entities such that every pattern edge maps to a data edge satisfying its
    /// predicate constraint, and returns the `k` highest-scoring matches.  When
    /// a pattern edge admits several parallel data edges (only possible with an
    /// unconstrained predicate), the representative is the maximum-weight one
    /// (ties broken by smallest predicate id), so each assignment yields exactly
    /// one scored match.
    ///
    /// Matching is connectivity-driven: each variable is seeded from a bound
    /// neighbour's adjacency, so the cost is roughly `O(V · d^{r-1})` for a
    /// connected `r`-variable pattern with maximum degree `d` (a disconnected
    /// component restarts at `O(V)`).  A branch-and-bound prune — current
    /// partial weight plus the per-edge maximum weight of unbound edges, against
    /// the current k-th best — cuts subtrees that cannot enter the top-k.
    /// Results are deterministic: score descending, ties by ascending
    /// assignment.
    pub fn topk_subgraph(&self, pattern: &QueryPattern, k: usize) -> Vec<SubgraphMatch> {
        self.run_topk(pattern, k, true, &|_vars, edges| {
            edges.iter().map(|e| e.weight).sum()
        })
    }

    /// Top-k subgraph matches scored by a caller-supplied function over the
    /// bound variables and matched edges.
    ///
    /// Same matching semantics as [`topk_subgraph`](Self::topk_subgraph), but
    /// the score is arbitrary, so weight-based branch-and-bound is disabled and
    /// the search fully enumerates structural matches (still pruned by
    /// connectivity and injectivity) before keeping the `k` best.  Complexity is
    /// `O(M)` in the number of structural matches `M`, plus `O(M log M)` to rank.
    pub fn topk_subgraph_by<F>(&self, pattern: &QueryPattern, k: usize, score: F) -> Vec<SubgraphMatch>
    where
        F: Fn(&[EntityLiteralId], &[MatchedEdge]) -> f64,
    {
        self.run_topk(pattern, k, false, &score)
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
    fn test_shortest_path_len_wrapper() {
        let g = GraphView::from_store(&make_store());
        assert_eq!(g.shortest_path_len(10, 13), Some(3));
        assert_eq!(g.shortest_path_len(10, 10), Some(0));
        assert_eq!(g.shortest_path_len(10, 20), None); // disconnected
        assert_eq!(g.shortest_path_len(999, 10), None); // absent node
    }

    #[test]
    fn test_khop_reachable_wrapper() {
        let g = GraphView::from_store(&make_store());
        // 10 → 13 is 3 hops away.
        assert_eq!(g.khop_reachable(10, 13, 3), Some(true));
        assert_eq!(g.khop_reachable(10, 13, 2), Some(false)); // budget too small
        assert_eq!(g.khop_reachable(10, 10, 0), Some(true)); // self is 0 hops
        assert_eq!(g.khop_reachable(10, 20, 99), Some(false)); // disconnected
        assert_eq!(g.khop_reachable(10, 999, 5), None); // absent node
    }

    #[test]
    fn test_has_cycle_wrapper() {
        // Full graph has a 3-cycle (triangle) and a self-loop ⇒ cyclic.
        let g = GraphView::from_store(&make_store());
        assert!(g.has_cycle());

        // A pure DAG (just the path component) is acyclic.
        let mut dag = TripleStore::new();
        dag.insert(IdTriple::new(1, 9, 2));
        dag.insert(IdTriple::new(2, 9, 3));
        dag.insert(IdTriple::new(1, 9, 3));
        assert!(!GraphView::from_store(&dag).has_cycle());

        // A lone self-loop is a cycle.
        let mut loopy = TripleStore::new();
        loopy.insert(IdTriple::new(7, 9, 7));
        assert!(GraphView::from_store(&loopy).has_cycle());
    }

    #[test]
    fn test_from_source_matches_from_store() {
        // `from_source` (generic over TripleSource) and `from_store` build the
        // same view: TripleStore is itself a TripleSource.
        let store = make_store();
        let a = GraphView::from_store(&store);
        let b = GraphView::from_source(&store);
        assert_eq!(a.node_count(), b.node_count());
        assert_eq!(a.edge_count(), b.edge_count());
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

    // ---- strongly-connected components ------------------------------------

    #[test]
    fn test_scc_directed_cycle_is_one() {
        // A directed cycle 1→2→3→4→1 is a single SCC.
        let mut s = TripleStore::new();
        let p = 1u32;
        s.insert(IdTriple::new(1, p, 2));
        s.insert(IdTriple::new(2, p, 3));
        s.insert(IdTriple::new(3, p, 4));
        s.insert(IdTriple::new(4, p, 1));
        let g = GraphView::from_store(&s);

        let (comp, count) = g.strongly_connected_components();
        assert_eq!(count, 1, "directed cycle collapses to one SCC");
        let c = comp[&1];
        assert_eq!(comp[&2], c);
        assert_eq!(comp[&3], c);
        assert_eq!(comp[&4], c);
    }

    #[test]
    fn test_scc_path_is_n_singletons() {
        // An acyclic chain of N nodes yields N singleton SCCs.
        let mut s = TripleStore::new();
        let p = 1u32;
        s.insert(IdTriple::new(1, p, 2));
        s.insert(IdTriple::new(2, p, 3));
        s.insert(IdTriple::new(3, p, 4));
        s.insert(IdTriple::new(4, p, 5));
        let g = GraphView::from_store(&s);

        let (comp, count) = g.strongly_connected_components();
        assert_eq!(count, 5, "a 5-node path has 5 SCCs");
        // Every node must have a distinct component id.
        let ids: std::collections::HashSet<usize> = comp.values().copied().collect();
        assert_eq!(ids.len(), 5);
    }

    #[test]
    fn test_scc_mixed_graph() {
        // make_store(): path 10→11→12→13 (4 SCCs), 3-cycle 20→21→22→20 (1 SCC),
        // self-loop 30 (1 singleton SCC). Total = 6 SCCs.
        let store = make_store();
        let g = GraphView::from_store(&store);
        let (comp, count) = g.strongly_connected_components();
        assert_eq!(count, 6);

        // The 3-cycle is one component.
        let c20 = comp[&20];
        assert_eq!(comp[&21], c20);
        assert_eq!(comp[&22], c20);

        // Path nodes are all distinct, and distinct from the cycle.
        assert_ne!(comp[&10], comp[&11]);
        assert_ne!(comp[&11], comp[&12]);
        assert_ne!(comp[&12], comp[&13]);
        assert_ne!(comp[&10], c20);

        // The self-looping node is its own singleton SCC.
        assert_ne!(comp[&30], c20);
    }

    // ---- betweenness centrality -------------------------------------------

    #[test]
    fn test_betweenness_star_hub_highest() {
        // Bidirectional star: hub 100 ↔ {1,2,3,4}. Every shortest path between
        // two leaves passes through the hub, so the hub has the highest score
        // and the leaves have zero.
        let mut s = TripleStore::new();
        let p = 1u32;
        let hub = 100u32;
        for leaf in [1u32, 2, 3, 4] {
            s.insert(IdTriple::new(hub, p, leaf));
            s.insert(IdTriple::new(leaf, p, hub));
        }
        let g = GraphView::from_store(&s);
        let bc = g.betweenness_centrality();

        let hub_score = bc[&hub];
        for leaf in [1u32, 2, 3, 4] {
            assert!(
                hub_score > bc[&leaf],
                "hub must outrank leaf {leaf} (hub={hub_score}, leaf={})",
                bc[&leaf]
            );
            // Leaves are never intermediate on any shortest path.
            assert!(bc[&leaf].abs() < 1e-9, "leaf {leaf} betweenness must be 0");
        }
        // k=4 leaves ⇒ hub lies on all 4·3 = 12 ordered leaf pairs.
        assert!((hub_score - 12.0).abs() < 1e-9, "hub score should be 12, got {hub_score}");
    }

    // ---- closeness centrality ---------------------------------------------

    #[test]
    fn test_closeness_path() {
        // Directed path 1→2→3→4 (n = 4).
        let mut s = TripleStore::new();
        let p = 1u32;
        s.insert(IdTriple::new(1, p, 2));
        s.insert(IdTriple::new(2, p, 3));
        s.insert(IdTriple::new(3, p, 4));
        let g = GraphView::from_store(&s);
        let cc = g.closeness_centrality();

        // Node 1 reaches {2,3,4}: r-1=3, S=1+2+3=6 ⇒ (3/6)·(3/3)=0.5.
        assert!((cc[&1] - 0.5).abs() < 1e-9, "got {}", cc[&1]);
        // Node 2 reaches {3,4}: r-1=2, S=1+2=3 ⇒ (2/3)·(2/3)=4/9.
        assert!((cc[&2] - (4.0 / 9.0)).abs() < 1e-9, "got {}", cc[&2]);
        // Node 3 reaches {4}: r-1=1, S=1 ⇒ (1/1)·(1/3)=1/3.
        assert!((cc[&3] - (1.0 / 3.0)).abs() < 1e-9, "got {}", cc[&3]);
        // Node 4 reaches nothing ⇒ 0.
        assert!(cc[&4].abs() < 1e-9, "got {}", cc[&4]);
    }

    // ---- Louvain community detection --------------------------------------

    #[test]
    fn test_louvain_two_cliques() {
        // Two K5 cliques (ids 1..=5 and 11..=15) joined by a single bridge
        // edge 5→11. Louvain should recover exactly the two cliques.
        let mut s = TripleStore::new();
        let p = 1u32;
        let clique_a = [1u32, 2, 3, 4, 5];
        let clique_b = [11u32, 12, 13, 14, 15];
        for clique in [&clique_a, &clique_b] {
            for i in 0..clique.len() {
                for j in (i + 1)..clique.len() {
                    s.insert(IdTriple::new(clique[i], p, clique[j]));
                    s.insert(IdTriple::new(clique[j], p, clique[i]));
                }
            }
        }
        s.insert(IdTriple::new(5, p, 11)); // single bridge

        let g = GraphView::from_store(&s);
        let comm = g.louvain();

        let distinct: std::collections::HashSet<usize> = comm.values().copied().collect();
        assert_eq!(distinct.len(), 2, "expected exactly two communities");

        // Every node of clique A shares one community...
        let ca = comm[&1];
        for &id in &clique_a {
            assert_eq!(comm[&id], ca, "clique-A node {id} drifted");
        }
        // ...and every node of clique B shares another.
        let cb = comm[&11];
        for &id in &clique_b {
            assert_eq!(comm[&id], cb, "clique-B node {id} drifted");
        }
        assert_ne!(ca, cb, "the two cliques must be different communities");
    }

    // ---- k-core decomposition ---------------------------------------------

    #[test]
    fn test_k_core_known_graph() {
        // K4 on {1,2,3,4} (core 3); node 5 attached to {1,2} (core 2);
        // node 6 attached to {1} (core 1).
        let mut s = TripleStore::new();
        let p = 1u32;
        // K4
        for (a, b) in [(1, 2), (1, 3), (1, 4), (2, 3), (2, 4), (3, 4)] {
            s.insert(IdTriple::new(a, p, b));
        }
        s.insert(IdTriple::new(5, p, 1));
        s.insert(IdTriple::new(5, p, 2));
        s.insert(IdTriple::new(6, p, 1));

        let g = GraphView::from_store(&s);
        let core = g.k_core();

        assert_eq!(core[&1], 3);
        assert_eq!(core[&2], 3);
        assert_eq!(core[&3], 3);
        assert_eq!(core[&4], 3);
        assert_eq!(core[&5], 2);
        assert_eq!(core[&6], 1);
    }

    #[test]
    fn test_k_core_triangle_with_pendant() {
        // Triangle {1,2,3} (core 2) plus pendant 4–1 (core 1).
        let mut s = TripleStore::new();
        let p = 1u32;
        s.insert(IdTriple::new(1, p, 2));
        s.insert(IdTriple::new(2, p, 3));
        s.insert(IdTriple::new(3, p, 1));
        s.insert(IdTriple::new(1, p, 4));
        let g = GraphView::from_store(&s);
        let core = g.k_core();

        assert_eq!(core[&1], 2);
        assert_eq!(core[&2], 2);
        assert_eq!(core[&3], 2);
        assert_eq!(core[&4], 1);
    }

    // =======================================================================
    // WeightedGraphView
    // =======================================================================

    /// Hand-checked weighted graph with two predicates.
    ///
    /// ```text
    /// p1 = 1 ("knows", weight 2):  1→2  2→3  3→4  4→5
    /// p2 = 2 ("shortcut", weight 1): 1→3  3→5
    /// ```
    ///
    /// All-predicate shortest paths from 1 prefer the p2 shortcuts; a p1-only
    /// filter forces the long chain; a p2-only filter isolates the shortcuts.
    fn make_weighted() -> WeightedGraphView {
        let mut s = TripleStore::new();
        // (sub, pred, obj, weight)
        let edges: [(u32, u32, u32, f64); 6] = [
            (1, 1, 2, 2.0),
            (2, 1, 3, 2.0),
            (3, 1, 4, 2.0),
            (4, 1, 5, 2.0),
            (1, 2, 3, 1.0),
            (3, 2, 5, 1.0),
        ];
        for &(a, p, b, _) in &edges {
            s.insert(IdTriple::new(a, p, b));
        }
        let wmap: HashMap<(u32, u32, u32), f64> =
            edges.iter().map(|&(a, p, b, w)| ((a, p, b), w)).collect();
        WeightedGraphView::from_store_with_weights(&s, move |t| wmap[&(t.sub, t.pred, t.obj)])
    }

    #[test]
    fn test_weighted_distances() {
        let g = make_weighted();
        let d = g.weighted_distances(1).expect("node 1 present");
        // 1→3 via p2 (1) is cheaper than 1→2→3 (4); 3→5 via p2 (1) gives 5 at 2.
        assert!(d[&1].abs() < 1e-9);
        assert!((d[&2] - 2.0).abs() < 1e-9);
        assert!((d[&3] - 1.0).abs() < 1e-9);
        assert!((d[&4] - 3.0).abs() < 1e-9, "got {}", d[&4]); // 1→3(1)→4(2)
        assert!((d[&5] - 2.0).abs() < 1e-9, "got {}", d[&5]); // 1→3(1)→5(1)
    }

    #[test]
    fn test_weighted_shortest_path() {
        let g = make_weighted();
        let (cost, path) = g.weighted_shortest_path(1, 5).expect("path 1⇝5 exists");
        assert!((cost - 2.0).abs() < 1e-9, "got {cost}");
        assert_eq!(path, vec![1, 3, 5]);

        // Trivial same-node path has zero cost.
        let (c0, p0) = g.weighted_shortest_path(1, 1).expect("self path");
        assert!(c0.abs() < 1e-9);
        assert_eq!(p0, vec![1]);

        // 5 is a sink: nothing is reachable from it.
        assert!(g.weighted_shortest_path(5, 1).is_none());
        // Unknown node.
        assert!(g.weighted_shortest_path(1, 999).is_none());
    }

    #[test]
    fn test_predicate_filtered_dijkstra() {
        let g = make_weighted();

        // p1 only: must walk the whole chain, weight 2 per hop.
        let only_p1: HashSet<PredId> = [1].into_iter().collect();
        let d1 = g.weighted_distances_filtered(1, &only_p1).expect("node 1");
        assert!((d1[&5] - 8.0).abs() < 1e-9, "got {}", d1[&5]); // 2+2+2+2
        let (c1, path1) = g.weighted_shortest_path_filtered(1, 5, &only_p1).unwrap();
        assert!((c1 - 8.0).abs() < 1e-9);
        assert_eq!(path1, vec![1, 2, 3, 4, 5]);

        // p2 only: only the shortcuts exist, so 2 and 4 are unreachable.
        let only_p2: HashSet<PredId> = [2].into_iter().collect();
        let d2 = g.weighted_distances_filtered(1, &only_p2).expect("node 1");
        assert!((d2[&3] - 1.0).abs() < 1e-9);
        assert!((d2[&5] - 2.0).abs() < 1e-9);
        assert!(!d2.contains_key(&2));
        assert!(!d2.contains_key(&4));
    }

    #[test]
    fn test_predicate_filtered_bfs() {
        let g = make_weighted();

        let only_p1: HashSet<PredId> = [1].into_iter().collect();
        let h1 = g.bfs_distances_filtered(1, &only_p1).expect("node 1");
        assert_eq!(h1[&1], 0);
        assert_eq!(h1[&2], 1);
        assert_eq!(h1[&3], 2);
        assert_eq!(h1[&4], 3);
        assert_eq!(h1[&5], 4);

        let only_p2: HashSet<PredId> = [2].into_iter().collect();
        let h2 = g.bfs_distances_filtered(1, &only_p2).expect("node 1");
        assert_eq!(h2[&1], 0);
        assert_eq!(h2[&3], 1);
        assert_eq!(h2[&5], 2);
        assert!(!h2.contains_key(&2));
    }

    #[test]
    fn test_weighted_pagerank_bias() {
        // S(1) points to X(2) with weight 3 and Y(3) with weight 1; both point
        // back to S. Weighted PageRank must rank X above Y (more of S's mass
        // flows to X), whereas the unweighted view ranks them equally.
        let mut s = TripleStore::new();
        let p = 1u32;
        s.insert(IdTriple::new(1, p, 2));
        s.insert(IdTriple::new(1, p, 3));
        s.insert(IdTriple::new(2, p, 1));
        s.insert(IdTriple::new(3, p, 1));
        let wmap: HashMap<(u32, u32, u32), f64> = [
            ((1u32, p, 2u32), 3.0),
            ((1, p, 3), 1.0),
            ((2, p, 1), 1.0),
            ((3, p, 1), 1.0),
        ]
        .into_iter()
        .collect();
        let wg = WeightedGraphView::from_store_with_weights(&s, move |t| wmap[&(t.sub, t.pred, t.obj)]);

        let prw = wg.weighted_pagerank(0.85, 200, 1e-12);
        assert!(prw[&2] > prw[&3] + 1e-9, "X({}) must outrank Y({})", prw[&2], prw[&3]);
        let sum: f64 = prw.values().sum();
        assert!((sum - 1.0).abs() < 1e-6, "weighted PR must sum to 1, got {sum}");

        // Unweighted: X and Y are symmetric, so their ranks coincide.
        let ug = GraphView::from_store(&s);
        let pru = ug.pagerank(0.85, 200, 1e-12);
        assert!((pru[&2] - pru[&3]).abs() < 1e-9, "unweighted X/Y must tie");
    }

    // ---- top-k subgraph query ---------------------------------------------

    /// Top-k path graph (predicate 1). Two-edge paths a→b→c and their summed
    /// weights:
    ///
    /// ```text
    /// (1,2,3)=10  (7,2,3)=8  (1,2,6)=7  (7,2,6)=5  (1,4,5)=2  (8,9,10)=10
    /// ```
    fn make_topk_paths() -> WeightedGraphView {
        let mut s = TripleStore::new();
        let edges: [(u32, u32, u32, f64); 8] = [
            (1, 1, 2, 5.0),
            (2, 1, 3, 5.0),
            (1, 1, 4, 1.0),
            (4, 1, 5, 1.0),
            (2, 1, 6, 2.0),
            (7, 1, 2, 3.0),
            (8, 1, 9, 5.0),
            (9, 1, 10, 5.0),
        ];
        for &(a, p, b, _) in &edges {
            s.insert(IdTriple::new(a, p, b));
        }
        let wmap: HashMap<(u32, u32, u32), f64> =
            edges.iter().map(|&(a, p, b, w)| ((a, p, b), w)).collect();
        WeightedGraphView::from_store_with_weights(&s, move |t| wmap[&(t.sub, t.pred, t.obj)])
    }

    /// Two-edge path pattern: v0 -p1-> v1 -p1-> v2.
    fn path_pattern() -> QueryPattern {
        QueryPattern::new(3, vec![PatternEdge::labeled(0, 1, 1), PatternEdge::labeled(1, 2, 1)])
    }

    #[test]
    fn test_topk_subgraph_default_scoring() {
        let g = make_topk_paths();
        let top = g.topk_subgraph(&path_pattern(), 3);
        assert_eq!(top.len(), 3);

        // (1,2,3) and (8,9,10) tie at 10 → lexicographic order; then (7,2,3)=8.
        assert_eq!(top[0].vars, vec![1, 2, 3]);
        assert!((top[0].score - 10.0).abs() < 1e-9);
        assert_eq!(top[1].vars, vec![8, 9, 10]);
        assert!((top[1].score - 10.0).abs() < 1e-9);
        assert_eq!(top[2].vars, vec![7, 2, 3]);
        assert!((top[2].score - 8.0).abs() < 1e-9);

        // Matched edges carry the right predicate and weight.
        assert_eq!(top[0].edges.len(), 2);
        assert_eq!(top[0].edges[0].src, 1);
        assert_eq!(top[0].edges[0].dst, 2);
        assert_eq!(top[0].edges[0].pred, 1);
        assert!((top[0].edges[0].weight - 5.0).abs() < 1e-9);
    }

    #[test]
    fn test_topk_ties_and_k_overflow() {
        let g = make_topk_paths();

        // k = 1 with a tie at 10 resolves deterministically to (1,2,3).
        let one = g.topk_subgraph(&path_pattern(), 1);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].vars, vec![1, 2, 3]);

        // k = 2 returns both score-10 matches in lexicographic order.
        let two = g.topk_subgraph(&path_pattern(), 2);
        assert_eq!(two.len(), 2);
        assert_eq!(two[0].vars, vec![1, 2, 3]);
        assert_eq!(two[1].vars, vec![8, 9, 10]);
        assert!((two[1].score - 10.0).abs() < 1e-9);

        // k far larger than the match count returns every match, fully ranked.
        let all = g.topk_subgraph(&path_pattern(), 100);
        assert_eq!(all.len(), 6);
        let scores: Vec<f64> = all.iter().map(|m| m.score).collect();
        assert!((scores[0] - 10.0).abs() < 1e-9);
        assert!((scores[5] - 2.0).abs() < 1e-9); // (1,4,5) is the weakest
        // Scores are non-increasing.
        for w in scores.windows(2) {
            assert!(w[0] >= w[1] - 1e-12);
        }

        // k = 0 is empty.
        assert!(g.topk_subgraph(&path_pattern(), 0).is_empty());
    }

    #[test]
    fn test_topk_custom_scoring() {
        // Score a path by the minimum of its two edge weights. (1,2,3) and
        // (8,9,10) tie at 5; lexicographic order keeps (1,2,3) on top.
        let g = make_topk_paths();
        let top = g.topk_subgraph_by(&path_pattern(), 1, |_vars, edges| {
            edges.iter().map(|e| e.weight).fold(f64::INFINITY, f64::min)
        });
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].vars, vec![1, 2, 3]);
        assert!((top[0].score - 5.0).abs() < 1e-9, "got {}", top[0].score);
    }

    #[test]
    fn test_topk_cycle_pattern() {
        // Two directed triangles of different weight. The cyclic pattern
        // v0→v1→v2→v0 must close the back-edge and rank the heavier triangle
        // first.
        let mut s = TripleStore::new();
        let edges: [(u32, u32, u32, f64); 6] = [
            (1, 1, 2, 1.0),
            (2, 1, 3, 1.0),
            (3, 1, 1, 1.0), // triangle A, total 3
            (4, 1, 5, 2.0),
            (5, 1, 6, 2.0),
            (6, 1, 4, 2.0), // triangle B, total 6
        ];
        for &(a, p, b, _) in &edges {
            s.insert(IdTriple::new(a, p, b));
        }
        let wmap: HashMap<(u32, u32, u32), f64> =
            edges.iter().map(|&(a, p, b, w)| ((a, p, b), w)).collect();
        let g = WeightedGraphView::from_store_with_weights(&s, move |t| wmap[&(t.sub, t.pred, t.obj)]);

        let pattern = QueryPattern::new(
            3,
            vec![
                PatternEdge::labeled(0, 1, 1),
                PatternEdge::labeled(1, 2, 1),
                PatternEdge::labeled(2, 0, 1),
            ],
        );

        // Each triangle yields its 3 rotations → 6 matches total.
        let all = g.topk_subgraph(&pattern, 100);
        assert_eq!(all.len(), 6);

        // Heaviest match is a rotation of triangle B; lexicographically (4,5,6).
        let top = g.topk_subgraph(&pattern, 1);
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].vars, vec![4, 5, 6]);
        assert!((top[0].score - 6.0).abs() < 1e-9);
    }

    #[test]
    fn test_topk_unconstrained_predicate_picks_max_weight() {
        // Parallel edges 1→2 under two predicates; an unconstrained pattern edge
        // selects the heavier one as the representative.
        let mut s = TripleStore::new();
        s.insert(IdTriple::new(1, 1, 2));
        s.insert(IdTriple::new(1, 2, 2));
        let wmap: HashMap<(u32, u32, u32), f64> =
            [((1u32, 1u32, 2u32), 4.0), ((1, 2, 2), 9.0)].into_iter().collect();
        let g = WeightedGraphView::from_store_with_weights(&s, move |t| wmap[&(t.sub, t.pred, t.obj)]);

        let pattern = QueryPattern::new(2, vec![PatternEdge::any(0, 1)]);
        let top = g.topk_subgraph(&pattern, 5);
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].vars, vec![1, 2]);
        assert_eq!(top[0].edges[0].pred, 2); // the weight-9 predicate wins
        assert!((top[0].score - 9.0).abs() < 1e-9);
    }
}
