//! Traversal building blocks over the [`CsrGraph`] for the graph-function
//! ports: reusable visited marks and an unweighted breadth-first search that
//! honours a [`PredFilter`], a [`Dir`]ection and an optional hop bound, in
//! upstream's neighbour order. (Every upstream graph function is unweighted;
//! there is no Dijkstra.)
//!
//! These are conveniences: an algorithm whose upstream loop shape differs
//! (bidirectional searches, per-level queues, `std::set`-ordered frontiers)
//! should be ported loop for loop on top of [`CsrGraph::try_for_each`] /
//! [`CsrGraph::out_neighbors`] / [`CsrGraph::in_neighbors`].

use std::ops::ControlFlow;

use super::{CsrGraph, Dir, GFunCtx, NodeId, PredFilter, PredId};
use crate::error::Result;

/// Per-node marks that reset in O(1): an epoch-stamped value per node, so one
/// allocation serves any number of searches (e.g. one BFS per source of a
/// large binding set).
#[derive(Debug, Clone)]
pub struct NodeMarks {
    stamp: Vec<u32>,
    value: Vec<u32>,
    epoch: u32,
}

impl NodeMarks {
    /// Marks for `n` nodes, all unset.
    pub fn new(n: usize) -> NodeMarks {
        NodeMarks {
            stamp: vec![0; n],
            value: vec![0; n],
            epoch: 1,
        }
    }

    /// Unset every mark (O(1) amortised).
    pub fn reset(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            self.stamp.iter_mut().for_each(|s| *s = 0);
            self.epoch = 1;
        }
    }

    /// The mark of `node`, if set since the last reset.
    #[inline]
    pub fn get(&self, node: NodeId) -> Option<u32> {
        let i = node as usize;
        (self.stamp[i] == self.epoch).then(|| self.value[i])
    }

    /// Whether `node` is marked.
    #[inline]
    pub fn is_set(&self, node: NodeId) -> bool {
        self.stamp[node as usize] == self.epoch
    }

    /// Mark `node` with `value`.
    #[inline]
    pub fn set(&mut self, node: NodeId, value: u32) {
        let i = node as usize;
        self.stamp[i] = self.epoch;
        self.value[i] = value;
    }
}

/// Sentinel parent of a BFS root.
pub const NO_PARENT: u32 = u32::MAX;

/// The result of [`bfs`]: nodes in discovery order with their hop distance and
/// the step that discovered them.
#[derive(Debug, Clone, Default)]
pub struct Bfs {
    /// Discovered nodes, start first, in FIFO discovery order.
    pub order: Vec<NodeId>,
    /// `dist[i]`: hops from the start to `order[i]`.
    pub dist: Vec<u32>,
    /// `parent[i]`: (index into `order` of the discoverer, predicate, base
    /// direction of the step); `(NO_PARENT, 0, Dir::Out)` for the start.
    pub parent: Vec<(u32, PredId, Dir)>,
}

impl Bfs {
    /// The discovery-tree path from the start to `order[idx]`, as
    /// `(node, predicate, base direction)` steps from the start's successor to
    /// the target (empty for the start itself).
    pub fn steps_to(&self, mut idx: usize) -> Vec<(NodeId, PredId, Dir)> {
        let mut rev = Vec::new();
        while self.parent[idx].0 != NO_PARENT {
            let (p, pred, d) = self.parent[idx];
            rev.push((self.order[idx], pred, d));
            idx = p as usize;
        }
        rev.reverse();
        rev
    }

    /// Node counts per distance (`[1, |layer 1|, ...]`).
    pub fn layer_sizes(&self) -> Vec<usize> {
        let mut v: Vec<usize> = Vec::new();
        for &d in &self.dist {
            let d = d as usize;
            if v.len() <= d {
                v.resize(d + 1, 0);
            }
            v[d] += 1;
        }
        v
    }
}

/// Breadth-first search from `start` in `dir` under `filter`, discovering
/// neighbours in upstream order (see [`CsrGraph::try_for_each`]) and marking a
/// node visited when it is first discovered (upstream's queue + visited-set
/// shape, e.g. `PathQueryHandler::BFS`, `SSSP`, `closenessCentrality`).
/// Nodes farther than `max_depth` hops are not discovered. `marks` must cover
/// the graph; it is reset first and afterwards maps each discovered node to its
/// index in [`Bfs::order`]. Checks for cancellation every 4096 expansions.
pub fn bfs(
    ctx: &GFunCtx<'_>,
    start: NodeId,
    filter: &PredFilter,
    dir: Dir,
    max_depth: Option<u32>,
    marks: &mut NodeMarks,
) -> Result<Bfs> {
    let g: &CsrGraph = ctx.csr;
    marks.reset();
    let mut out = Bfs::default();
    out.order.push(start);
    out.dist.push(0);
    out.parent.push((NO_PARENT, 0, Dir::Out));
    marks.set(start, 0);
    let mut head = 0usize;
    while head < out.order.len() {
        if head % 4096 == 4095 {
            ctx.check_cancel()?;
        }
        let v = out.order[head];
        let d = out.dist[head];
        if max_depth.is_some_and(|m| d >= m) {
            head += 1;
            continue;
        }
        let _ = g.try_for_each::<(), _>(v, filter, dir, |n, p, step| {
            if !marks.is_set(n) {
                marks.set(n, out.order.len() as u32);
                out.order.push(n);
                out.dist.push(d + 1);
                out.parent.push((head as u32, p, step));
            }
            ControlFlow::Continue(())
        });
        head += 1;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::Dictionary;

    #[test]
    fn bfs_orders_layers_and_paths() {
        // 0 -0-> 1 -0-> 2, 3 -0-> 1
        let g = CsrGraph::from_triples(&[(0, 0, 1), (1, 0, 2), (3, 0, 1)]).unwrap();
        let dict = Dictionary::new();
        let ctx = GFunCtx::new(&g, &dict);
        let all = PredFilter::all(1);
        let mut marks = NodeMarks::new(g.num_nodes());
        let r = bfs(&ctx, 0, &all, Dir::Out, None, &mut marks).unwrap();
        assert_eq!(r.order, vec![0, 1, 2]);
        assert_eq!(r.layer_sizes(), vec![1, 1, 1]);
        assert_eq!(r.steps_to(2), vec![(1, 0, Dir::Out), (2, 0, Dir::Out)]);
        let r = bfs(&ctx, 0, &all, Dir::OutIn, None, &mut marks).unwrap();
        assert_eq!(r.order, vec![0, 1, 2, 3]);
        assert_eq!(r.parent[3], (1, 0, Dir::In));
        assert_eq!(marks.get(3), Some(3));
        let r = bfs(&ctx, 0, &all, Dir::OutIn, Some(1), &mut marks).unwrap();
        assert_eq!(r.order, vec![0, 1]);
        assert!(!marks.is_set(2));
    }
}
