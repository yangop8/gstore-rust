//! Unweighted traversal building blocks over a [`CsrGraph`] with a
//! [`PredFilter`] and a [`Direction`] (gStore's graph functions are all
//! unweighted: BFS, never Dijkstra).
//!
//! Upstream algorithms differ in *how they interleave* the predicates and the
//! two edge directions when they expand a vertex, and that order decides
//! which of several equally short paths / which first neighbour wins. The
//! three patterns that occur are captured by [`NbrOrder`]; pick the one the
//! upstream function uses (each stub's docs say which):
//!
//! | [`NbrOrder`]               | upstream loop                                                     | used by (examples)                                   |
//! |----------------------------|-------------------------------------------------------------------|------------------------------------------------------|
//! | `PerPredOutThenIn`         | `for pred: { out(pred); if !directed: in(pred) }`                 | shortestPath, SSSP(Len), closeness, PR, kHopEnumerate |
//! | `PerPredInThenOut`         | `for pred: { in(pred); if !directed: out(pred) }`                 | `PathQueryHandler::BFS`, WCC, labelProp (in first)   |
//! | `AllInThenAllOut`          | `for pred: in(pred)` then `for pred: out(pred)` (undirected only) | bfsCount, kHopNeighbor, shortestPathCount            |
//!
//! With [`Direction::Out`] every order degenerates to "for pred: out(pred)";
//! with [`Direction::In`] to "for pred: in(pred)".

use std::ops::ControlFlow;

use crate::analytics::csr::{CsrGraph, Direction, EdgeDir, Node, PredFilter};
use crate::model::id::PredId;

/// How a vertex expansion interleaves predicates and edge directions (see the
/// module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NbrOrder {
    /// For each predicate: out-neighbours, then in-neighbours.
    PerPredOutThenIn,
    /// For each predicate: in-neighbours, then out-neighbours.
    PerPredInThenOut,
    /// All predicates' in-neighbours, then all predicates' out-neighbours.
    AllInThenAllOut,
}

/// Visit the neighbours of `node` reachable over one edge in `dir`, in the
/// given upstream [`NbrOrder`]. Neighbours reached over several edges are
/// visited once per edge (no dedup — upstream does not dedup either).
pub fn try_for_each_nbr<B, F>(
    g: &CsrGraph,
    node: Node,
    filter: &PredFilter,
    dir: Direction,
    order: NbrOrder,
    mut f: F,
) -> ControlFlow<B>
where
    F: FnMut(Node, PredId, EdgeDir) -> ControlFlow<B>,
{
    match dir {
        Direction::Out => g.try_for_each_out(node, filter, |n, p| f(n, p, EdgeDir::Out)),
        Direction::In => g.try_for_each_in(node, filter, |n, p| f(n, p, EdgeDir::In)),
        Direction::Both => match order {
            NbrOrder::PerPredOutThenIn => g.try_for_each_both(node, filter, f),
            NbrOrder::PerPredInThenOut => {
                for &p in filter.preds() {
                    for &n in g.in_nbrs(node, p) {
                        f(n, p, EdgeDir::In)?;
                    }
                    for &n in g.out_nbrs(node, p) {
                        f(n, p, EdgeDir::Out)?;
                    }
                }
                ControlFlow::Continue(())
            }
            NbrOrder::AllInThenAllOut => {
                g.try_for_each_in(node, filter, |n, p| f(n, p, EdgeDir::In))?;
                g.try_for_each_out(node, filter, |n, p| f(n, p, EdgeDir::Out))
            }
        },
    }
}

/// Non-breaking form of [`try_for_each_nbr`].
pub fn for_each_nbr<F>(
    g: &CsrGraph,
    node: Node,
    filter: &PredFilter,
    dir: Direction,
    order: NbrOrder,
    mut f: F,
) where
    F: FnMut(Node, PredId, EdgeDir),
{
    let _ = try_for_each_nbr(g, node, filter, dir, order, |n, p, d| {
        f(n, p, d);
        ControlFlow::<()>::Continue(())
    });
}

/// A visited-set over CSR nodes with `O(1)` reset (generation stamps), for
/// running many traversals (one per source) without reallocating.
#[derive(Debug, Clone)]
pub struct NodeMarks {
    stamp: Vec<u32>,
    cur: u32,
}

impl NodeMarks {
    /// Marks for `n` nodes, all clear.
    pub fn new(n: usize) -> NodeMarks {
        NodeMarks {
            stamp: vec![0; n],
            cur: 1,
        }
    }

    /// Clear every mark in `O(1)` (amortised).
    pub fn clear(&mut self) {
        if self.cur == u32::MAX {
            self.stamp.iter_mut().for_each(|s| *s = 0);
            self.cur = 1;
        } else {
            self.cur += 1;
        }
    }

    /// Whether `n` is marked.
    pub fn is_marked(&self, n: Node) -> bool {
        self.stamp.get(n as usize) == Some(&self.cur)
    }

    /// Mark `n`; returns `true` if it was not marked before.
    pub fn mark(&mut self, n: Node) -> bool {
        match self.stamp.get_mut(n as usize) {
            Some(s) if *s != self.cur => {
                *s = self.cur;
                true
            }
            _ => false,
        }
    }
}

/// Distance value of a node a BFS did not reach.
pub const UNREACHED: u32 = u32::MAX;

/// Breadth-first search from `src` (FIFO queue; a node is marked when it is
/// *enqueued*, like every upstream BFS), visiting each reached node once with
/// its hop distance, in discovery order (`src` first, depth 0). Expansion
/// stops below `max_depth` hops when given. `visit` may stop the search early.
/// Returns the visit's break value, if any.
pub fn bfs<B, F>(
    g: &CsrGraph,
    src: Node,
    filter: &PredFilter,
    dir: Direction,
    order: NbrOrder,
    max_depth: Option<u32>,
    mut visit: F,
) -> ControlFlow<B>
where
    F: FnMut(Node, u32) -> ControlFlow<B>,
{
    if src as usize >= g.num_nodes() {
        return ControlFlow::Continue(());
    }
    let mut marks = NodeMarks::new(g.num_nodes());
    let mut queue: std::collections::VecDeque<(Node, u32)> = std::collections::VecDeque::new();
    marks.mark(src);
    queue.push_back((src, 0));
    visit(src, 0)?;
    while let Some((v, d)) = queue.pop_front() {
        if max_depth.is_some_and(|m| d >= m) {
            continue;
        }
        try_for_each_nbr(g, v, filter, dir, order, |n, _, _| {
            if marks.mark(n) {
                queue.push_back((n, d + 1));
                visit(n, d + 1)?;
            }
            ControlFlow::Continue(())
        })?;
    }
    ControlFlow::Continue(())
}

/// Hop distances from `src` to every node ([`UNREACHED`] when unreachable),
/// indexed by node. Distances do not depend on the [`NbrOrder`].
pub fn bfs_levels(g: &CsrGraph, src: Node, filter: &PredFilter, dir: Direction) -> Vec<u32> {
    let mut dist = vec![UNREACHED; g.num_nodes()];
    let _ = bfs(
        g,
        src,
        filter,
        dir,
        NbrOrder::PerPredOutThenIn,
        None,
        |n, d| {
            dist[n as usize] = d;
            ControlFlow::<()>::Continue(())
        },
    );
    dist
}

/// The `(node, depth)` discovery order of a BFS from `src`.
pub fn bfs_order(
    g: &CsrGraph,
    src: Node,
    filter: &PredFilter,
    dir: Direction,
    order: NbrOrder,
) -> Vec<(Node, u32)> {
    let mut out = Vec::new();
    let _ = bfs(g, src, filter, dir, order, None, |n, d| {
        out.push((n, d));
        ControlFlow::<()>::Continue(())
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::IdTriple;

    /// 0 -p0-> 1, 2 -p0-> 0, 0 -p1-> 3, 4 -p1-> 0, 1 -p0-> 5
    fn g() -> CsrGraph {
        CsrGraph::from_id_triples(vec![
            IdTriple::new(0, 0, 1),
            IdTriple::new(2, 0, 0),
            IdTriple::new(0, 1, 3),
            IdTriple::new(4, 1, 0),
            IdTriple::new(1, 0, 5),
        ])
        .unwrap()
    }

    fn order_of(order: NbrOrder) -> Vec<(Node, PredId, EdgeDir)> {
        let g = g();
        let mut v = Vec::new();
        for_each_nbr(
            &g,
            0,
            &PredFilter::from_ids([0, 1]),
            Direction::Both,
            order,
            |n, p, d| v.push((n, p, d)),
        );
        v
    }

    #[test]
    fn neighbour_orders() {
        use EdgeDir::{In, Out};
        assert_eq!(
            order_of(NbrOrder::PerPredOutThenIn),
            vec![(1, 0, Out), (2, 0, In), (3, 1, Out), (4, 1, In)]
        );
        assert_eq!(
            order_of(NbrOrder::PerPredInThenOut),
            vec![(2, 0, In), (1, 0, Out), (4, 1, In), (3, 1, Out)]
        );
        assert_eq!(
            order_of(NbrOrder::AllInThenAllOut),
            vec![(2, 0, In), (4, 1, In), (1, 0, Out), (3, 1, Out)]
        );
    }

    #[test]
    fn bfs_levels_and_order() {
        let g = g();
        let f = PredFilter::from_ids([0, 1]);
        let d = bfs_levels(&g, 0, &f, Direction::Out);
        assert_eq!(d, vec![0, 1, UNREACHED, 1, UNREACHED, 2]);
        let o = bfs_order(&g, 5, &f, Direction::Both, NbrOrder::PerPredOutThenIn);
        assert_eq!(o, vec![(5, 0), (1, 1), (0, 2), (2, 3), (3, 3), (4, 3)]);
        let mut seen = 0;
        let r = bfs(
            &g,
            0,
            &f,
            Direction::Both,
            NbrOrder::AllInThenAllOut,
            Some(1),
            |n, _| {
                seen += 1;
                if n == 4 {
                    ControlFlow::Break(n)
                } else {
                    ControlFlow::Continue(())
                }
            },
        );
        assert_eq!(r, ControlFlow::Break(4));
        assert_eq!(seen, 3); // 0, then in-neighbours 2 and 4
        assert!(bfs_order(&g, 99, &f, Direction::Out, NbrOrder::PerPredOutThenIn).is_empty());
    }

    #[test]
    fn node_marks_reset() {
        let mut m = NodeMarks::new(3);
        assert!(m.mark(1));
        assert!(!m.mark(1));
        assert!(m.is_marked(1));
        m.clear();
        assert!(!m.is_marked(1));
        assert!(!m.mark(7), "out of range is never marked");
    }
}
