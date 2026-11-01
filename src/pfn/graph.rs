//! The graph a PFN function sees ([`GraphCtx`]) — upstream's `GAnalysis`
//! (`src/Pfn/GAnalysis.h`: id ↔ string through the KVstore) plus the
//! `CSRQueryHandler` it hands out (vertex / edge / predicate counts, per
//! predicate out- and in-neighbours).
//!
//! # Ids
//!
//! * **Vertices** are the CSR's dense node indices `0..vertex_count()` —
//!   upstream's functions loop `for (vid = 0; vid < getVertNum(); ++vid)`
//!   over (assumed dense) entity ids; here every vertex is visited exactly
//!   once whatever the entity ids are (see [`crate::analytics::csr`]). Only
//!   entities incident to an entity→entity edge are vertices; triples with a
//!   literal object are not edges.
//! * **Terms** are dictionary entity / literal ids (upstream's
//!   `TYPE_ENTITY_LITERAL_ID`); [`GraphCtx::vertex_term`] /
//!   [`GraphCtx::term_vertex`] convert. Term strings are in the dictionary's
//!   N-Triples form: `<http://ex/a>`, `_:b`, `"text"`, `"1"^^<…#integer>`.
//! * **Predicates** are dictionary predicate ids;
//!   [`GraphCtx::predicates`] lists those labelling at least one edge.
//!
//! The neighbours of a vertex via one predicate are ascending; "all
//! predicates" lists edges by `(predicate, neighbour)`, so an `(s, o)` pair
//! linked by two predicates appears twice (upstream's multi-edges).

use crate::analytics::csr::CsrGraph;
use crate::dict::Dictionary;
use crate::model::id::PredId;

/// Read access to the graph for a PFN function (object safe, so any runtime
/// can hold a `&dyn GraphCtx`).
pub trait GraphCtx {
    /// Number of vertices (upstream `getVertNum()`).
    fn vertex_count(&self) -> u64;
    /// Number of edges, an `(s, o)` pair counted once per predicate
    /// (upstream `getEdgeNum()`).
    fn edge_count(&self) -> u64;
    /// The predicates labelling at least one edge, ascending.
    fn predicates(&self) -> &[PredId];
    /// Out-degree of vertex `v` via `pred` (every predicate when `None`);
    /// `None` for a vertex out of range.
    fn out_degree(&self, v: u64, pred: Option<PredId>) -> Option<u64>;
    /// In-degree, as [`out_degree`](Self::out_degree).
    fn in_degree(&self, v: u64, pred: Option<PredId>) -> Option<u64>;
    /// The `i`-th out-neighbour of `v` via `pred` (ascending), or in
    /// `(predicate, neighbour)` order over every predicate when `None`.
    fn out_neighbor(&self, v: u64, pred: Option<PredId>, i: u64) -> Option<u64>;
    /// The `i`-th in-neighbour, as [`out_neighbor`](Self::out_neighbor).
    fn in_neighbor(&self, v: u64, pred: Option<PredId>, i: u64) -> Option<u64>;
    /// The predicate of the `i`-th out-edge of `v` in all-predicate order
    /// (pairs with `out_neighbor(v, None, i)`).
    fn out_edge_pred(&self, v: u64, i: u64) -> Option<PredId>;
    /// The predicate of the `i`-th in-edge of `v` in all-predicate order.
    fn in_edge_pred(&self, v: u64, i: u64) -> Option<PredId>;
    /// The term (entity id) of vertex `v`.
    fn vertex_term(&self, v: u64) -> Option<u64>;
    /// The vertex of term `t`, if it is one.
    fn term_vertex(&self, t: u64) -> Option<u64>;
    /// The N-Triples string of term `t` (entity or literal).
    fn term_str(&self, t: u64) -> Option<&str>;
    /// The term id of an N-Triples string (`"…"` → a literal, else an
    /// entity), if the dictionary has it.
    fn term_id(&self, s: &str) -> Option<u64>;
    /// The id of predicate IRI `s` (`<…>`).
    fn pred_id(&self, s: &str) -> Option<PredId>;
    /// The `<…>` string of predicate `p`.
    fn pred_str(&self, p: PredId) -> Option<&str>;
}

/// [`GraphCtx`] over a database's CSR ([`crate::Database::csr`]) and its
/// dictionary.
#[derive(Debug, Clone, Copy)]
pub struct CsrGraphCtx<'a> {
    csr: &'a CsrGraph,
    dict: &'a Dictionary,
}

impl<'a> CsrGraphCtx<'a> {
    pub fn new(csr: &'a CsrGraph, dict: &'a Dictionary) -> CsrGraphCtx<'a> {
        CsrGraphCtx { csr, dict }
    }

    fn node(&self, v: u64) -> Option<u32> {
        u32::try_from(v)
            .ok()
            .filter(|&n| (n as usize) < self.csr.num_nodes())
    }

    fn degree(&self, v: u64, pred: Option<PredId>, out: bool) -> Option<u64> {
        let n = self.node(v)?;
        Some(match (pred, out) {
            (Some(p), true) => self.csr.out_nbrs(n, p).len(),
            (Some(p), false) => self.csr.in_nbrs(n, p).len(),
            (None, true) => self.csr.out_edges(n).1.len(),
            (None, false) => self.csr.in_edges(n).1.len(),
        } as u64)
    }

    fn neighbor(&self, v: u64, pred: Option<PredId>, i: u64, out: bool) -> Option<u64> {
        let n = self.node(v)?;
        let list = match (pred, out) {
            (Some(p), true) => self.csr.out_nbrs(n, p),
            (Some(p), false) => self.csr.in_nbrs(n, p),
            (None, true) => self.csr.out_edges(n).1,
            (None, false) => self.csr.in_edges(n).1,
        };
        usize::try_from(i)
            .ok()
            .and_then(|i| list.get(i))
            .map(|&x| u64::from(x))
    }

    fn edge_pred(&self, v: u64, i: u64, out: bool) -> Option<PredId> {
        let n = self.node(v)?;
        let preds = if out {
            self.csr.out_edges(n).0
        } else {
            self.csr.in_edges(n).0
        };
        usize::try_from(i).ok().and_then(|i| preds.get(i)).copied()
    }
}

impl GraphCtx for CsrGraphCtx<'_> {
    fn vertex_count(&self) -> u64 {
        self.csr.num_nodes() as u64
    }

    fn edge_count(&self) -> u64 {
        self.csr.num_edges()
    }

    fn predicates(&self) -> &[PredId] {
        self.csr.predicates()
    }

    fn out_degree(&self, v: u64, pred: Option<PredId>) -> Option<u64> {
        self.degree(v, pred, true)
    }

    fn in_degree(&self, v: u64, pred: Option<PredId>) -> Option<u64> {
        self.degree(v, pred, false)
    }

    fn out_neighbor(&self, v: u64, pred: Option<PredId>, i: u64) -> Option<u64> {
        self.neighbor(v, pred, i, true)
    }

    fn in_neighbor(&self, v: u64, pred: Option<PredId>, i: u64) -> Option<u64> {
        self.neighbor(v, pred, i, false)
    }

    fn out_edge_pred(&self, v: u64, i: u64) -> Option<PredId> {
        self.edge_pred(v, i, true)
    }

    fn in_edge_pred(&self, v: u64, i: u64) -> Option<PredId> {
        self.edge_pred(v, i, false)
    }

    fn vertex_term(&self, v: u64) -> Option<u64> {
        self.node(v).map(|n| u64::from(self.csr.entity_of(n)))
    }

    fn term_vertex(&self, t: u64) -> Option<u64> {
        let t = u32::try_from(t).ok()?;
        self.csr.node_of(t).map(u64::from)
    }

    fn term_str(&self, t: u64) -> Option<&str> {
        self.dict.id_to_string(u32::try_from(t).ok()?)
    }

    fn term_id(&self, s: &str) -> Option<u64> {
        let id = if s.starts_with('"') {
            self.dict.literal_id(s)
        } else {
            self.dict.entity_id(s)
        };
        id.map(u64::from)
    }

    fn pred_id(&self, s: &str) -> Option<PredId> {
        self.dict.predicate_id(s)
    }

    fn pred_str(&self, p: PredId) -> Option<&str> {
        self.dict.predicate_to_string(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;

    #[test]
    fn csr_context_answers_the_graph_questions() {
        let db = Database::build_from_str(
            "pfn_graph",
            "<http://ex/a> <http://ex/p> <http://ex/b> .\n\
             <http://ex/a> <http://ex/q> <http://ex/b> .\n\
             <http://ex/a> <http://ex/p> <http://ex/c> .\n\
             <http://ex/c> <http://ex/p> <http://ex/a> .\n\
             <http://ex/a> <http://ex/name> \"A\" .\n",
        )
        .unwrap();
        let csr = db.csr().unwrap();
        let g = CsrGraphCtx::new(&csr, db.dict());
        assert_eq!(g.vertex_count(), 3);
        assert_eq!(g.edge_count(), 4);
        let p = g.pred_id("<http://ex/p>").unwrap();
        let q = g.pred_id("<http://ex/q>").unwrap();
        assert_eq!(
            g.predicates().len(),
            2,
            "the literal-valued predicate has no edge"
        );
        assert_eq!(g.pred_str(p), Some("<http://ex/p>"));
        let a = g.term_vertex(g.term_id("<http://ex/a>").unwrap()).unwrap();
        let b = g.term_vertex(g.term_id("<http://ex/b>").unwrap()).unwrap();
        assert_eq!(g.out_degree(a, None), Some(3));
        assert_eq!(g.out_degree(a, Some(p)), Some(2));
        assert_eq!(g.out_degree(a, Some(q)), Some(1));
        assert_eq!(g.in_degree(b, None), Some(2));
        assert_eq!(g.in_degree(a, Some(p)), Some(1));
        assert_eq!(g.out_degree(99, None), None);
        assert_eq!(g.out_neighbor(a, Some(q), 0), Some(b));
        assert_eq!(g.out_neighbor(a, Some(q), 1), None);
        // All predicates: (predicate, neighbour) order, multi-edges kept.
        let all: Vec<(u64, PredId)> = (0..3)
            .map(|i| {
                (
                    g.out_neighbor(a, None, i).unwrap(),
                    g.out_edge_pred(a, i).unwrap(),
                )
            })
            .collect();
        let mut sorted = all.clone();
        sorted.sort_by_key(|&(n, pr)| (pr, n));
        assert_eq!(all, sorted);
        assert_eq!(
            g.in_edge_pred(b, 0).map(|pr| pr == p || pr == q),
            Some(true)
        );
        let term = g.vertex_term(a).unwrap();
        assert_eq!(g.term_str(term), Some("<http://ex/a>"));
        assert!(g.term_id("\"A\"").is_some());
        assert_eq!(g.term_vertex(g.term_id("\"A\"").unwrap()), None);
        assert_eq!(g.term_id("<http://ex/none>"), None);
    }
}
