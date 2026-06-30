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
//! # Deferred work
//! * Weighted / labeled edge variants (predicate-aware)
//! * Directed-modularity Louvain (current implementation symmetrises edges)
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
}
