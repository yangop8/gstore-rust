//! Node-based query planner: a port of gStore's `PlanGenerator`.
//!
//! It models the BGP as gStore does — a graph of so-variable *nodes* connected
//! by edges — and produces a join order via:
//!
//! * **NodeScore heuristic** (`HeuristicFirstNode`/`HeuristicNextNode`):
//!   `1 + Σ PARAM_PRE/(pred_card+1) + PARAM_SIZE/(candidate_size+1)`, choosing
//!   the most-constrained, most-connected node first and growing the frontier.
//! * **Sampling-based cardinality** (`CardEstimator`/`EstimateOneEdgeSelectivity`):
//!   for larger queries, the next node is the frontier node whose *sampled* join
//!   cardinality with the current plan is smallest — probing real edges through
//!   the store rather than assuming independence.
//! * **Satellite-node deferral**: degree-1 nodes are planned last (they only
//!   expand output, never enable further joins).
//!
//! The resulting variable order is bridged to the pattern order the executor
//! consumes. Constants from gStore: `SAMPLE_PRO`, `SAMPLE_CACHE_MAX`,
//! `SMALL_QUERY_VAR_NUM`, `PARAM_SIZE`, `PARAM_PRE`.

use std::collections::HashSet;

use crate::store::TripleSource;

use super::candidates::Candidates;
use super::engine::{PatPlan, Slot};

const SMALL_QUERY_VAR_NUM: usize = 4;
const SAMPLE_CACHE_MAX: usize = 50;
const PARAM_SIZE: f64 = 1_000_000.0;
const PARAM_PRE: f64 = 10_000.0;

/// One incident edge of a variable: the connecting predicate and the neighbour
/// variable (if the other endpoint is also a variable).
struct Edge {
    pred: Slot,
    /// The neighbour so-variable, if the other endpoint is a variable.
    neighbor: Option<usize>,
    /// True if this variable is the subject of the edge (out-edge).
    is_subject: bool,
}

/// The query graph derived from compiled patterns.
struct Graph {
    num_vars: usize,
    edges: Vec<Vec<Edge>>, // edges[v] = incident edges of var v (as an so-node)
    is_so_var: Vec<bool>,  // var appears in subject/object position
}

impl Graph {
    fn build(plans: &[PatPlan], num_vars: usize) -> Graph {
        let mut edges: Vec<Vec<Edge>> = (0..num_vars).map(|_| Vec::new()).collect();
        let mut is_so_var = vec![false; num_vars];
        for plan in plans.iter() {
            if let Slot::Var(sv) = plan.s {
                is_so_var[sv] = true;
                edges[sv].push(Edge {
                    pred: plan.p,
                    neighbor: plan.o.var(),
                    is_subject: true,
                });
            }
            if let Slot::Var(ov) = plan.o {
                is_so_var[ov] = true;
                edges[ov].push(Edge {
                    pred: plan.p,
                    neighbor: plan.s.var(),
                    is_subject: false,
                });
            }
        }
        Graph {
            num_vars,
            edges,
            is_so_var,
        }
    }

    fn degree(&self, v: usize) -> usize {
        self.edges[v].len()
    }
}

/// Compute the pattern evaluation order for a BGP.
pub(crate) fn plan(
    plans: &[PatPlan],
    store: &impl TripleSource,
    candidates: &Candidates,
    num_vars: usize,
) -> Vec<usize> {
    if plans.len() <= 1 {
        return (0..plans.len()).collect();
    }
    let graph = Graph::build(plans, num_vars);

    // so-vars split into join nodes and satellite (degree-1) nodes.
    let so_vars: Vec<usize> = (0..num_vars).filter(|&v| graph.is_so_var[v]).collect();
    let join_vars: Vec<usize> = so_vars
        .iter()
        .copied()
        .filter(|&v| graph.degree(v) > 1)
        .collect();
    let satellite_vars: Vec<usize> = so_vars
        .iter()
        .copied()
        .filter(|&v| graph.degree(v) == 1)
        .collect();

    // Variable cardinality (exact candidate size, else whole-DB estimate).
    let var_num = |v: usize| -> usize {
        candidates
            .get(&v)
            .map(|c| c.len())
            .unwrap_or_else(|| whole_db_size(store, &graph, v))
    };

    // Connected nodes to actually order: if there are join nodes use them, else
    // fall back to all so-vars (e.g. a single 2-var triple is all "satellite").
    let nodes: Vec<usize> = if join_vars.is_empty() {
        so_vars.clone()
    } else {
        join_vars.clone()
    };

    // With no so-vars to order (e.g. a BGP whose patterns are all
    // `<const> ?p <const>` — only predicates are variables), leave the order
    // empty; `bridge_to_pattern_order` then emits every pattern via its trailing
    // "remaining patterns" pass. The ordering routines require a non-empty slice
    // (they take a max/min first node and would panic on an empty one).
    let var_order = if nodes.is_empty() {
        Vec::new()
    } else if nodes.len() <= SMALL_QUERY_VAR_NUM {
        heuristic_order(&graph, &nodes, store, candidates, &var_num)
    } else {
        sampling_order(&graph, &nodes, store, candidates, &var_num)
    };

    // Append satellite nodes not already covered (deferred to the end).
    let mut full_order = var_order;
    for &s in &satellite_vars {
        if !full_order.contains(&s) {
            full_order.push(s);
        }
    }
    for &v in &so_vars {
        if !full_order.contains(&v) {
            full_order.push(v);
        }
    }

    bridge_to_pattern_order(plans, &full_order, store, candidates)
}

/// gStore `NodeScore`: prefer small candidate sets and selective connections to
/// already-placed nodes.
fn node_score(
    graph: &Graph,
    v: usize,
    placed: &HashSet<usize>,
    store: &impl TripleSource,
    var_num: &impl Fn(usize) -> usize,
) -> f64 {
    let mut score = 1.0;
    for e in &graph.edges[v] {
        if let Some(nei) = e.neighbor {
            if placed.contains(&nei) {
                let pre_size = match e.pred {
                    Slot::Const(p) => store.pred_card(p).max(1),
                    Slot::Var(_) => {
                        (store.triple_count() as usize / store.num_predicates().max(1)).max(2)
                    }
                };
                score += PARAM_PRE / (pre_size as f64 + 1.0);
            }
        }
    }
    score += PARAM_SIZE / (var_num(v) as f64 + 1.0);
    score
}

/// Greedy NodeScore growth (gStore `HeuristicPlan`).
fn heuristic_order(
    graph: &Graph,
    nodes: &[usize],
    store: &impl TripleSource,
    _candidates: &Candidates,
    var_num: &impl Fn(usize) -> usize,
) -> Vec<usize> {
    let node_set: HashSet<usize> = nodes.iter().copied().collect();
    let mut placed: HashSet<usize> = HashSet::new();
    let mut order = Vec::new();

    // First node: highest NodeScore (empty plan ⇒ just the candidate-size term).
    let first = *nodes
        .iter()
        .max_by(|&&a, &&b| {
            node_score(graph, a, &placed, store, var_num)
                .partial_cmp(&node_score(graph, b, &placed, store, var_num))
                .unwrap()
        })
        .unwrap();
    order.push(first);
    placed.insert(first);

    let mut frontier = neighbors_in(graph, first, &node_set, &placed);
    while order.len() < nodes.len() {
        let next = frontier
            .iter()
            .copied()
            .max_by(|&a, &b| {
                node_score(graph, a, &placed, store, var_num)
                    .partial_cmp(&node_score(graph, b, &placed, store, var_num))
                    .unwrap()
            })
            // disconnected component: take any remaining node
            .or_else(|| nodes.iter().copied().find(|v| !placed.contains(v)));
        let Some(next) = next else { break };
        order.push(next);
        placed.insert(next);
        frontier.remove(&next);
        for n in neighbors_in(graph, next, &node_set, &placed) {
            frontier.insert(n);
        }
    }
    order
}

/// Sampling-based growth (gStore DP `CardEstimator`): pick the next frontier
/// node whose sampled join cardinality with the current plan is smallest.
fn sampling_order(
    graph: &Graph,
    nodes: &[usize],
    store: &impl TripleSource,
    candidates: &Candidates,
    var_num: &impl Fn(usize) -> usize,
) -> Vec<usize> {
    let node_set: HashSet<usize> = nodes.iter().copied().collect();
    let mut placed: HashSet<usize> = HashSet::new();

    // Pre-sample each node's candidate set.
    let samples: Vec<Vec<u32>> = (0..graph.num_vars)
        .map(|v| match candidates.get(&v) {
            Some(c) => id_cache_sample(c),
            None => Vec::new(),
        })
        .collect();

    // First node: smallest cardinality.
    let first = *nodes.iter().min_by_key(|&&v| var_num(v)).unwrap();
    let mut order = vec![first];
    placed.insert(first);
    let mut card = var_num(first) as f64;

    let mut frontier = neighbors_in(graph, first, &node_set, &placed);
    while order.len() < nodes.len() {
        let pick = frontier
            .iter()
            .copied()
            .map(|w| {
                let sel = best_edge_selectivity(graph, w, &placed, store, candidates, &samples);
                let new_card = (card * sel).max(1.0);
                (w, new_card)
            })
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
        let (next, new_card) = match pick {
            Some(p) => p,
            None => match nodes.iter().copied().find(|v| !placed.contains(v)) {
                Some(v) => (v, card * var_num(v) as f64),
                None => break,
            },
        };
        order.push(next);
        placed.insert(next);
        card = new_card;
        frontier.remove(&next);
        for n in neighbors_in(graph, next, &node_set, &placed) {
            frontier.insert(n);
        }
    }
    order
}

/// The minimum sampled selectivity over `w`'s edges to already-placed nodes.
fn best_edge_selectivity(
    graph: &Graph,
    w: usize,
    placed: &HashSet<usize>,
    store: &impl TripleSource,
    candidates: &Candidates,
    samples: &[Vec<u32>],
) -> f64 {
    let mut best = f64::INFINITY;
    for e in &graph.edges[w] {
        let Some(nei) = e.neighbor else { continue };
        if !placed.contains(&nei) {
            continue;
        }
        // Sample from the already-placed neighbour, probe the edge, and measure
        // the average fan-out that lands in w's candidate set.
        let sample = &samples[nei];
        if sample.is_empty() {
            // No sample ⇒ fall back to a coarse predicate-based estimate.
            let est = match e.pred {
                Slot::Const(p) => {
                    store.pred_card(p) as f64 / store.distinct_subjects().max(1) as f64
                }
                Slot::Var(_) => 1.0,
            };
            best = best.min(est.max(0.01));
            continue;
        }
        // For the neighbour as the driver, w is reached across the edge.
        // If w is the subject of its edge, the neighbour is the object, so we
        // step object→subject; otherwise subject→object.
        let mut pass = 0usize;
        let w_cand = candidates.get(&w);
        for &drv in sample {
            let reached: Vec<u32> = if e.is_subject {
                // w is subject, neighbour is object ⇒ subjects of (pred?, drv)
                match e.pred {
                    Slot::Const(p) => store.s_by_po(p, drv),
                    Slot::Var(_) => store.ps_by_o(drv).iter().map(|&(_, s)| s).collect(),
                }
            } else {
                // w is object, neighbour is subject ⇒ objects of (drv, pred?)
                match e.pred {
                    Slot::Const(p) => store.o_by_sp(drv, p),
                    Slot::Var(_) => store.po_by_s(drv).iter().map(|&(_, o)| o).collect(),
                }
            };
            pass += match w_cand {
                Some(c) => reached
                    .iter()
                    .filter(|x| c.binary_search(x).is_ok())
                    .count(),
                None => reached.len(),
            };
        }
        let sel = (pass as f64 / sample.len() as f64).max(0.0001);
        best = best.min(sel);
    }
    if best.is_finite() {
        best
    } else {
        1.0
    }
}

/// gStore `GetIdCacheSample`: evenly-strided sample of a candidate list.
fn id_cache_sample(cache: &[u32]) -> Vec<u32> {
    let size = cache.len();
    let sample_size = if size <= 100 {
        size.min(SAMPLE_CACHE_MAX)
    } else {
        ((size as f64).ln() * 11.0) as usize
    }
    .max(1);
    if sample_size >= size {
        return cache.to_vec();
    }
    let stride = (size / sample_size).max(1);
    cache.iter().step_by(stride).copied().collect()
}

/// Whole-DB size estimate for a variable with no candidates.
fn whole_db_size(store: &impl TripleSource, graph: &Graph, v: usize) -> usize {
    // If the variable is ever a subject it's an entity; otherwise it may also be
    // a literal, so include the object population.
    let only_subject = graph.edges[v].iter().all(|e| e.is_subject);
    if only_subject {
        store.distinct_subjects().max(1)
    } else {
        (store.distinct_subjects() + store.distinct_objects()).max(1)
    }
}

/// Neighbour nodes of `v` that are in `node_set` and not yet placed.
fn neighbors_in(
    graph: &Graph,
    v: usize,
    node_set: &HashSet<usize>,
    placed: &HashSet<usize>,
) -> HashSet<usize> {
    let mut out = HashSet::new();
    for e in &graph.edges[v] {
        if let Some(n) = e.neighbor {
            if node_set.contains(&n) && !placed.contains(&n) {
                out.insert(n);
            }
        }
    }
    out
}

/// Convert a variable join order into a pattern evaluation order: a pattern is
/// emitted once all its so-variables are bound (or being introduced). When
/// several patterns become ready at the same step, the most selective (smallest
/// candidate-capped index size) is emitted first.
fn bridge_to_pattern_order(
    plans: &[PatPlan],
    var_order: &[usize],
    store: &impl TripleSource,
    candidates: &Candidates,
) -> Vec<usize> {
    let mut bound: HashSet<usize> = HashSet::new();
    let mut emitted = vec![false; plans.len()];
    let mut order = Vec::with_capacity(plans.len());

    let so_vars_of = |plan: &PatPlan| -> Vec<usize> {
        let mut v = Vec::new();
        if let Slot::Var(s) = plan.s {
            v.push(s);
        }
        if let Slot::Var(o) = plan.o {
            v.push(o);
        }
        v
    };

    for &v in var_order {
        bound.insert(v);
        // Collect patterns that became ready at this step, cheapest first.
        let mut ready: Vec<usize> = (0..plans.len())
            .filter(|&i| {
                if emitted[i] {
                    return false;
                }
                let svars = so_vars_of(&plans[i]);
                svars.contains(&v) && svars.iter().all(|x| bound.contains(x))
            })
            .collect();
        ready.sort_by_key(|&i| pattern_cost(&plans[i], store, candidates));
        for i in ready {
            order.push(i);
            emitted[i] = true;
        }
    }
    // Any remaining patterns (all-constant, or disconnected) go last.
    for (i, e) in emitted.iter().enumerate() {
        if !e {
            order.push(i);
        }
    }
    order
}

/// The index-scan size a pattern would fetch (used to order patterns that
/// become ready at the same step — fetch the smallest index first; the
/// candidate filter then prunes the results).
fn pattern_cost(plan: &PatPlan, store: &impl TripleSource, _candidates: &Candidates) -> u64 {
    match (plan.s, plan.p, plan.o) {
        (Slot::Const(_), Slot::Const(_), Slot::Const(_)) => 1,
        (Slot::Const(s), Slot::Const(p), Slot::Var(_)) => store.o_by_sp(s, p).len() as u64,
        (Slot::Var(_), Slot::Const(p), Slot::Const(o)) => store.s_by_po(p, o).len() as u64,
        (Slot::Const(s), Slot::Var(_), Slot::Const(o)) => store.p_by_so(s, o).len() as u64,
        (Slot::Const(s), Slot::Var(_), Slot::Var(_)) => store.po_by_s(s).len() as u64,
        (Slot::Var(_), Slot::Const(p), Slot::Var(_)) => store.so_by_p(p).len() as u64,
        (Slot::Var(_), Slot::Var(_), Slot::Const(o)) => store.ps_by_o(o).len() as u64,
        (Slot::Var(_), Slot::Var(_), Slot::Var(_)) => store.triple_count(),
    }
    .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::Dictionary;
    use crate::model::{IdTriple, Term};
    use crate::query::candidates;
    use crate::store::TripleStore;

    fn pp(s: Slot, p: Slot, o: Slot) -> PatPlan {
        PatPlan { s, p, o }
    }

    /// star: ?x type Grad(100); ?x takesCourse course0(200). Only entity 2 matches both.
    fn skewed() -> (Dictionary, TripleStore) {
        let mut d = Dictionary::new();
        let _ = d.intern_predicate(&Term::iri("type").dict_key()); // 0
        let _ = d.intern_predicate(&Term::iri("takes").dict_key()); // 1
        let mut s = TripleStore::new();
        let mut t = vec![IdTriple::new(2, 1, 200), IdTriple::new(5, 1, 200)];
        for e in 0..2000u32 {
            t.push(IdTriple::new(e, 0, 100)); // 2000 grad students
        }
        s.bulk_load(t);
        (d, s)
    }

    #[test]
    fn plans_selective_pattern_first() {
        let (_d, s) = skewed();
        // ?x(0) type(0) Grad(100) [card 2000] ; ?x(0) takes(1) course0(200) [card 2]
        let plans = vec![
            pp(Slot::Var(0), Slot::Const(0), Slot::Const(100)),
            pp(Slot::Var(0), Slot::Const(1), Slot::Const(200)),
        ];
        let cands = candidates::generate(&plans, &s);
        // ?x candidate = intersection = {2} → both patterns equally constrained by it.
        let order = plan(&plans, &s, &cands, 1);
        assert_eq!(order.len(), 2);
        // The more selective standalone pattern (takesCourse, card 2) should lead.
        assert_eq!(order[0], 1);
    }

    #[test]
    fn satellite_node_is_deferred() {
        // ?x knows ?y . ?x knows ?z . ?y type T  → ?z is a degree-1 leaf.
        let mut d = Dictionary::new();
        let knows = d.intern_predicate(&Term::iri("knows").dict_key());
        let typ = d.intern_predicate(&Term::iri("type").dict_key());
        let mut s = TripleStore::new();
        s.bulk_load(vec![
            IdTriple::new(1, knows, 2),
            IdTriple::new(1, knows, 3),
            IdTriple::new(2, typ, 100),
        ]);
        // vars: x=0, y=1, z=2
        let plans = vec![
            pp(Slot::Var(0), Slot::Const(knows), Slot::Var(1)),
            pp(Slot::Var(0), Slot::Const(knows), Slot::Var(2)),
            pp(Slot::Var(1), Slot::Const(typ), Slot::Const(100)),
        ];
        let cands = candidates::generate(&plans, &s);
        let order = plan(&plans, &s, &cands, 3);
        // The leaf pattern (?x knows ?z, introducing degree-1 ?z) should be last.
        assert_eq!(*order.last().unwrap(), 1);
    }

    #[test]
    fn store_statistics_are_exposed() {
        let (_d, s) = skewed();
        // predicate 0 = type (2000 triples), 1 = takes (2)
        assert_eq!(s.pred_card(0), 2000);
        assert_eq!(s.pred_card(1), 2);
        assert_eq!(s.pred_distinct_subj(0), 2000);
        assert_eq!(s.num_predicates(), 2);
    }

    #[test]
    fn order_is_permutation() {
        let (_d, s) = skewed();
        let plans = vec![
            pp(Slot::Var(0), Slot::Const(0), Slot::Const(100)),
            pp(Slot::Var(0), Slot::Const(1), Slot::Var(1)),
        ];
        let cands = candidates::generate(&plans, &s);
        let mut order = plan(&plans, &s, &cands, 2);
        order.sort_unstable();
        assert_eq!(order, vec![0, 1]);
    }
}
