//! Cost-based join optimizer — a port of gStore's `PlanGenerator` DP optimizer.
//!
//! Where [`super::planner`] grows a join order greedily (NodeScore heuristic +
//! sampling), this module runs a genuine **dynamic-programming plan
//! enumeration** over subsets of the BGP's triple patterns, exactly as gStore's
//! optimizer does:
//!
//! * **Left-deep DP** ([`dp_left_deep`]) finds the optimal *pipelined* join
//!   order — `dp[S]` = the cheapest way to materialise the patterns in `S`, built
//!   by appending one connected pattern at a time. This drop-in replaces the
//!   greedy order for the executor's existing left-deep pipeline.
//! * **Bushy DP** ([`dp_bushy`], gStore's `ConsiderBinaryJoin`) additionally
//!   enumerates every partition of `S` into two connected halves, so a query
//!   shaped like *two stars joined by a bridge* can be evaluated as a **binary
//!   join** of two independently-built sub-results — often far cheaper than any
//!   left-deep order. When the optimal plan is genuinely bushy it is returned as
//!   a [`JoinTree`] the executor runs with hash joins.
//!
//! The **cost model** is the textbook System-R estimator: a pattern's
//! cardinality comes from the index it would scan, and a join's output
//! cardinality is `|A|·|B| / max(NDV_A(v), NDV_B(v))` over the shared join
//! variables `v`, where the *number of distinct values* `NDV` is read from the
//! predicate statistics (`pre2sub`/`pre2obj` ⇒ [`TripleStore::pred_distinct_subj`]
//! /[`pred_distinct_obj`](TripleStore::pred_distinct_obj)) and tightened by the
//! exact candidate sets. The DP tables themselves are the *plan cache*: every
//! sub-plan's optimal cost is memoised once and reused across all supersets.

use std::collections::HashMap;

use crate::store::TripleSource;

use super::candidates::Candidates;
use super::engine::{PatPlan, Slot};

/// A physical join tree. A `Leaf` scans one triple pattern from scratch; a
/// `Join` hash-joins the results of two independent sub-trees (a *binary* join).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JoinTree {
    Leaf(usize),
    Join(Box<JoinTree>, Box<JoinTree>),
}

impl JoinTree {
    /// A tree is left-deep when every join's right operand is a single leaf — in
    /// that case the executor's pipelined left-deep loop already realises it, so
    /// no binary-join tree is needed.
    pub(crate) fn is_left_deep(&self) -> bool {
        match self {
            JoinTree::Leaf(_) => true,
            JoinTree::Join(l, r) => matches!(**r, JoinTree::Leaf(_)) && l.is_left_deep(),
        }
    }
}

/// The chosen execution plan: a left-deep pattern `order` (consumed by the
/// pipelined executor), plus — when a bushy plan is strictly cheaper — a binary
/// join `tree` to execute instead.
#[derive(Debug, Clone)]
pub(crate) struct ExecPlan {
    pub(crate) order: Vec<usize>,
    pub(crate) tree: Option<JoinTree>,
}

/// Largest BGP (in patterns) for which the `n·2ⁿ` left-deep DP runs; beyond it we
/// fall back to the greedy [`super::planner::plan`].
const LEFTDEEP_DP_LIMIT: usize = 14;
/// Largest BGP for which the `3ⁿ` bushy DP runs (binary-join consideration).
const BUSHY_DP_LIMIT: usize = 10;

/// Compute the execution plan for a BGP: optimal left-deep order, plus a bushy
/// binary-join tree when one is strictly cheaper.
pub(crate) fn optimize(
    plans: &[PatPlan],
    store: &impl TripleSource,
    candidates: &Candidates,
    num_vars: usize,
) -> ExecPlan {
    let n = plans.len();
    if n <= 1 {
        return ExecPlan {
            order: (0..n).collect(),
            tree: None,
        };
    }
    // The DP indexes patterns by bit and variables by bit; both must fit a mask.
    if n > LEFTDEEP_DP_LIMIT || num_vars > 64 {
        return ExecPlan {
            order: super::planner::plan(plans, store, candidates, num_vars),
            tree: None,
        };
    }

    let model = CostModel::build(plans, store, candidates);

    // Optimal left-deep order (the executor's default path) and its cost.
    let (order, left_deep_cost) = dp_left_deep(n, &model);

    // Bushy DP: only worthwhile for ≥4 patterns and within the 3ⁿ budget. Use a
    // binary-join tree only when it is genuinely bushy *and* strictly cheaper.
    let tree = if (4..=BUSHY_DP_LIMIT).contains(&n) {
        let (cost_bushy, tree) = dp_bushy(n, &model);
        if !tree.is_left_deep() && cost_bushy < left_deep_cost {
            Some(tree)
        } else {
            None
        }
    } else {
        None
    };

    ExecPlan { order, tree }
}

/// Per-BGP cost inputs: pattern cardinalities, per-pattern join-variable bitmasks
/// and NDV maps, and the subset→variable-union memo. Mirrors gStore's
/// statistics gathered once before plan enumeration.
struct CostModel {
    /// `card[i]` — estimated number of triples matching pattern `i`.
    card: Vec<f64>,
    /// `ndv[i][v]` — distinct values variable `v` takes in pattern `i`.
    ndv: Vec<HashMap<usize, f64>>,
    /// `subset_vars[mask]` — union of each pattern's join-variable bitmask over
    /// the patterns in `mask`.
    subset_vars: Vec<u64>,
}

impl CostModel {
    fn build(plans: &[PatPlan], store: &impl TripleSource, candidates: &Candidates) -> CostModel {
        let n = plans.len();
        let mut pvars = vec![0u64; n];
        let mut card = vec![1.0; n];
        let mut ndv = vec![HashMap::new(); n];
        for (i, plan) in plans.iter().enumerate() {
            for v in plan_vars(plan) {
                pvars[i] |= 1u64 << v;
            }
            card[i] = pattern_card(plan, store);
            ndv[i] = build_ndv(plan, store, candidates);
        }
        // subset → variable union (built bottom-up over the lowest set bit).
        let mut subset_vars = vec![0u64; 1usize << n];
        for mask in 1..(1usize << n) {
            let low = mask & mask.wrapping_neg();
            let i = low.trailing_zeros() as usize;
            subset_vars[mask] = subset_vars[mask ^ low] | pvars[i];
        }
        CostModel {
            card,
            ndv,
            subset_vars,
        }
    }

    /// Minimum NDV of variable `v` across the patterns in `mask` (the most
    /// selective pattern pins how many distinct values `v` can take).
    fn subset_ndv(&self, mask: usize, v: usize) -> f64 {
        let mut best = f64::INFINITY;
        let mut bits = mask;
        while bits != 0 {
            let i = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            if let Some(&d) = self.ndv[i].get(&v) {
                if d < best {
                    best = d;
                }
            }
        }
        if best.is_finite() {
            best
        } else {
            1.0
        }
    }

    /// Estimated output cardinality of joining sub-results over masks `a` and `b`
    /// (System-R: `|A|·|B| / Π max(NDV_A(v), NDV_B(v))` over shared vars).
    fn join_card(&self, a: usize, b: usize, card_a: f64, card_b: f64) -> f64 {
        let shared = self.subset_vars[a] & self.subset_vars[b];
        if shared == 0 {
            return card_a * card_b; // disconnected ⇒ cross product
        }
        let mut denom = 1.0;
        let mut bits = shared;
        while bits != 0 {
            let v = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            denom *= self.subset_ndv(a, v).max(self.subset_ndv(b, v)).max(1.0);
        }
        (card_a * card_b / denom).max(1.0)
    }
}

/// Left-deep DP: `dp[mask]` = (min cost, output card, last pattern, prev mask).
/// Returns the optimal pattern order and the full-set cost.
fn dp_left_deep(n: usize, model: &CostModel) -> (Vec<usize>, f64) {
    let size = 1usize << n;
    let mut cost = vec![f64::INFINITY; size];
    let mut card = vec![f64::INFINITY; size];
    let mut last = vec![usize::MAX; size];
    let mut prev = vec![0usize; size];

    for i in 0..n {
        let m = 1usize << i;
        cost[m] = model.card[i];
        card[m] = model.card[i];
        last[m] = i;
    }

    for mask in 1..size {
        if mask.count_ones() < 2 {
            continue; // singletons already seeded
        }
        let mut bits = mask;
        while bits != 0 {
            let i = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let pm = mask ^ (1usize << i);
            if !cost[pm].is_finite() {
                continue;
            }
            let jc = model.join_card(pm, 1usize << i, card[pm], model.card[i]);
            let c = cost[pm] + jc;
            if c < cost[mask] {
                cost[mask] = c;
                card[mask] = jc;
                last[mask] = i;
                prev[mask] = pm;
            }
        }
    }

    let full = size - 1;
    // Reconstruct the order by unwinding `last`/`prev` from the full set.
    let mut order = Vec::with_capacity(n);
    let mut m = full;
    while m != 0 {
        order.push(last[m]);
        m = prev[m];
    }
    order.reverse();
    (order, cost[full])
}

/// Bushy DP: `dp[mask]` = (min cost, output card, tree). Enumerates every
/// partition of `mask` into two halves (gStore `ConsiderBinaryJoin`).
fn dp_bushy(n: usize, model: &CostModel) -> (f64, JoinTree) {
    let size = 1usize << n;
    let mut cost = vec![f64::INFINITY; size];
    let mut card = vec![f64::INFINITY; size];
    let mut tree: Vec<Option<JoinTree>> = vec![None; size];

    for i in 0..n {
        let m = 1usize << i;
        cost[m] = model.card[i];
        card[m] = model.card[i];
        tree[m] = Some(JoinTree::Leaf(i));
    }

    for mask in 1..size {
        if mask.count_ones() < 2 {
            continue;
        }
        let low = mask & mask.wrapping_neg();
        // Enumerate proper submasks `l` containing the lowest bit; `r = mask\l`.
        let mut sub = (mask - 1) & mask;
        while sub != 0 {
            if sub & low != 0 {
                let l = sub;
                let r = mask ^ sub;
                if r != 0 && cost[l].is_finite() && cost[r].is_finite() {
                    let jc = model.join_card(l, r, card[l], card[r]);
                    let c = cost[l] + cost[r] + jc;
                    if c < cost[mask] {
                        cost[mask] = c;
                        card[mask] = jc;
                        tree[mask] = Some(JoinTree::Join(
                            Box::new(tree[l].clone().unwrap()),
                            Box::new(tree[r].clone().unwrap()),
                        ));
                    }
                }
            }
            sub = (sub - 1) & mask;
        }
    }

    let full = size - 1;
    (cost[full], tree[full].clone().unwrap_or(JoinTree::Leaf(0)))
}

// --- cost-model statistics --------------------------------------------------

/// Distinct variable indices appearing in a pattern's slots.
pub(crate) fn plan_vars(plan: &PatPlan) -> Vec<usize> {
    let mut v = Vec::with_capacity(3);
    for slot in [plan.s, plan.p, plan.o] {
        if let Slot::Var(idx) = slot {
            if !v.contains(&idx) {
                v.push(idx);
            }
        }
    }
    v
}

/// Estimated number of triples matching a pattern's constants — the size of the
/// index range the executor would scan for it.
fn pattern_card(plan: &PatPlan, store: &impl TripleSource) -> f64 {
    use Slot::{Const, Var};
    let c = match (plan.s, plan.p, plan.o) {
        (Const(_), Const(_), Const(_)) => 1usize,
        (Const(s), Const(p), Var(_)) => store.o_by_sp(s, p).len(),
        (Var(_), Const(p), Const(o)) => store.s_by_po(p, o).len(),
        (Const(s), Var(_), Const(o)) => store.p_by_so(s, o).len(),
        (Const(s), Var(_), Var(_)) => store.po_by_s(s).len(),
        (Var(_), Const(p), Var(_)) => store.so_by_p(p).len(),
        (Var(_), Var(_), Const(o)) => store.ps_by_o(o).len(),
        (Var(_), Var(_), Var(_)) => store.triple_count() as usize,
    };
    (c as f64).max(1.0)
}

/// Per-variable NDV (number of distinct values) for one pattern, from predicate
/// statistics and tightened by the exact candidate sets.
fn build_ndv(plan: &PatPlan, store: &impl TripleSource, cand: &Candidates) -> HashMap<usize, f64> {
    use Slot::{Const, Var};
    let mut m: HashMap<usize, f64> = HashMap::new();
    let mut put = |v: usize, d: usize| {
        let mut d = (d as f64).max(1.0);
        if let Some(c) = cand.get(&v) {
            d = d.min((c.len() as f64).max(1.0));
        }
        let e = m.entry(v).or_insert(d);
        if d < *e {
            *e = d;
        }
    };

    if let Var(v) = plan.s {
        let d = match (plan.p, plan.o) {
            (Const(p), Const(o)) => store.s_by_po(p, o).len(),
            (Const(p), Var(_)) => store.pred_distinct_subj(p),
            (Var(_), Const(o)) => distinct_first(&store.ps_by_o(o)), // distinct subjects of o
            (Var(_), Var(_)) => store.distinct_subjects(),
        };
        put(v, d);
    }
    if let Var(v) = plan.o {
        let d = match (plan.s, plan.p) {
            (Const(s), Const(p)) => store.o_by_sp(s, p).len(),
            (Var(_), Const(p)) => store.pred_distinct_obj(p),
            (Const(s), Var(_)) => store.po_by_s(s).len(), // objects of s (upper bound)
            (Var(_), Var(_)) => store.distinct_objects(),
        };
        put(v, d);
    }
    if let Var(v) = plan.p {
        let d = match (plan.s, plan.o) {
            (Const(s), Const(o)) => store.p_by_so(s, o).len(),
            (Const(s), Var(_)) => distinct_first(&store.po_by_s(s)), // distinct preds of s
            (Var(_), Const(o)) => distinct_first(&store.ps_by_o(o)), // distinct preds of o
            (Var(_), Var(_)) => store.num_predicates(),
        };
        put(v, d);
    }
    m
}

/// Count distinct first components of `(a, b)` pairs sorted by `(a, b)`.
fn distinct_first(pairs: &[(u32, u32)]) -> usize {
    let mut count = 0usize;
    let mut last: Option<u32> = None;
    for &(a, _) in pairs {
        if last != Some(a) {
            count += 1;
            last = Some(a);
        }
    }
    count
}

