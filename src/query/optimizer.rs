//! Cost-based join optimizer — a port of gStore's `PlanGenerator` DP optimizer.
//!
//! Where [`super::planner`] grows a join order greedily (NodeScore heuristic +
//! sampling), this module runs a genuine **dynamic-programming plan
//! enumeration** over subsets of the BGP's triple patterns, exactly as gStore's
//! optimizer does:
//!
//! * **Left-deep DP** (`dp_left_deep`) finds the optimal *pipelined* join
//!   order — `dp[S]` = the cheapest way to materialise the patterns in `S`, built
//!   by appending one connected pattern at a time. This drop-in replaces the
//!   greedy order for the executor's existing left-deep pipeline.
//! * **Bushy DP** (`dp_bushy`, gStore's `ConsiderBinaryJoin`) additionally
//!   enumerates every partition of `S` into two connected halves, so a query
//!   shaped like *two stars joined by a bridge* can be evaluated as a **binary
//!   join** of two independently-built sub-results — often far cheaper than any
//!   left-deep order. When the optimal plan is genuinely bushy it is returned as
//!   a `JoinTree` the executor runs with hash joins.
//!
//! The **cost model** is the textbook System-R estimator: a pattern's
//! cardinality comes from the index it would scan, and a join's output
//! cardinality is `|A|·|B| / max(NDV_A(v), NDV_B(v))` over the shared join
//! variables `v`, where the *number of distinct values* `NDV` is read from the
//! predicate statistics (`pre2sub`/`pre2obj` ⇒ [`TripleSource::pred_distinct_subj`]
//! /[`pred_distinct_obj`](TripleSource::pred_distinct_obj)) and tightened by the
//! exact candidate sets. The DP tables themselves are the *plan cache*: every
//! sub-plan's optimal cost is memoised once and reused across all supersets.
//!
//! # Bounded, cancellable search
//!
//! Beyond the DP's reach (more than 14 patterns) a greedy order is refined by
//! 2-opt local search, whose naive cost is cubic in the BGP's size (review
//! query#13: a 200-pattern star took seconds to plan, and a cancel could not
//! stop it). The search is therefore bounded by [`OPT_WORK_BUDGET`] work
//! units — one unit per pattern placement it scores — skipped above
//! [`MAX_2OPT_PATTERNS`] patterns (the greedy order is used as is), and
//! polls the evaluator's stop condition (cancel flag, refused `SERVICE`,
//! failed sub-`SELECT`) every [`POLL_EVERY`] units: a stopped search returns
//! the best order found so far, and the evaluator reports the cancel. The
//! work of every search of a thread is counted ([`work_on_this_thread`]) so
//! tests can bound it without timing it.

use std::cell::{Cell, RefCell};
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

/// The physical method used to join two materialised intermediate results — a
/// port of gStore `Join::judge`, which picks per *pair* of operands between a
/// multi-way/hash join and an index/nested-loop join according to the operand
/// sizes and the query's structure.
///
/// Here both operands are materialised binding vectors:
///
/// * [`JoinMethod::Hash`] builds a hash table on the shared-variable key of one
///   side and probes it with the other — `O(|L| + |R|)`, the general method.
/// * [`JoinMethod::NestedLoop`] scans the two sides directly with no hash table
///   — `O(|L| · |R|)`, but with a tiny constant and no allocation, so it wins
///   when one side is very small / highly selective (the classic index-nested-
///   loop regime when an operand is a single value or a handful of rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JoinMethod {
    /// Hash join (gStore `multi_join`): the general-purpose method.
    Hash,
    /// Nested-loop join (gStore `index_join`): for a tiny / very selective side.
    NestedLoop,
}

/// Choose a join method from the two operand sizes (gStore `Join::judge`).
///
/// The cost model compares the nested-loop cost `min·max` against the hash cost
/// `|L| + |R|` (build one side, probe with the other) and picks nested-loop only
/// when it is no more expensive — which, since `min·max ≤ |L|+|R|` forces
/// `min ≈ 1`, means "exactly when one side is tiny / highly selective". A
/// general join of two sizeable results always picks the linear hash join.
pub(crate) fn judge(left_len: usize, right_len: usize) -> JoinMethod {
    let smaller = left_len.min(right_len);
    let bigger = left_len.max(right_len);
    let nested_cost = smaller.saturating_mul(bigger);
    let hash_cost = left_len.saturating_add(right_len);
    if nested_cost <= hash_cost {
        JoinMethod::NestedLoop
    } else {
        JoinMethod::Hash
    }
}

/// Largest BGP (in patterns) for which the `n·2ⁿ` left-deep DP runs; beyond it we
/// fall back to the greedy [`super::planner::plan`].
const LEFTDEEP_DP_LIMIT: usize = 14;
/// Largest BGP for which the `3ⁿ` bushy DP runs (binary-join consideration).
const BUSHY_DP_LIMIT: usize = 10;

/// The most work units ([`work_on_this_thread`]) the 2-opt refinement of one
/// large BGP may spend: one unit per pattern placement it scores. 2²¹ units
/// cover all sixteen passes over a 64-pattern BGP; a larger one stops part
/// way, keeping the best order found (never worse than the greedy seed).
pub const OPT_WORK_BUDGET: u64 = 1 << 21;

/// Above this many patterns the greedy order is used without 2-opt: one swap
/// alone scores every pattern, so the budget would not even cover the swaps
/// of the first position.
pub const MAX_2OPT_PATTERNS: usize = 256;

/// Work units between two polls of the stop condition.
pub const POLL_EVERY: u64 = 4096;

thread_local! {
    /// The plan-search work of this thread (see [`work_on_this_thread`]).
    static WORK: Cell<u64> = const { Cell::new(0) };
    /// The plans with a bushy tree this thread chose (see
    /// [`bushy_plans_on_this_thread`]).
    static BUSHY_CHOSEN: Cell<u64> = const { Cell::new(0) };
    /// `false`: this thread's plans are left-deep only (see
    /// [`set_bushy_plans_on_this_thread`]).
    static BUSHY_ALLOWED: Cell<bool> = const { Cell::new(true) };
    /// Called at every poll of a search on this thread (see
    /// [`set_poll_hook_on_this_thread`]).
    static POLL_HOOK: RefCell<Option<Box<dyn FnMut()>>> = const { RefCell::new(None) };
}

/// The work units every plan search of this thread spent so far: the
/// left-deep DP's `n·2ⁿ` and the bushy DP's `3ⁿ` steps, and one per pattern
/// placement the 2-opt refinement scored. A work counter for tests (review
/// query#13: bounded by [`OPT_WORK_BUDGET`] per large BGP however many
/// patterns it has); the evaluator plans on the thread that runs the query.
#[doc(hidden)]
pub fn work_on_this_thread() -> u64 {
    WORK.try_with(Cell::get).unwrap_or(0)
}

/// How many plans with a bushy (binary-join) tree the optimizer chose on
/// this thread so far. For tests (review tests-b#14): an end-to-end test of
/// the bushy executor can check the plan it meant to drive was chosen.
#[doc(hidden)]
pub fn bushy_plans_on_this_thread() -> u64 {
    BUSHY_CHOSEN.try_with(Cell::get).unwrap_or(0)
}

/// Let (`true`, the default) or forbid the plans made on this thread to use
/// a bushy tree; forbidden, every plan is the optimal left-deep one. For
/// tests comparing the bushy executor with the left-deep pipeline (review
/// tests-b#14). Returns the previous setting.
#[doc(hidden)]
pub fn set_bushy_plans_on_this_thread(allowed: bool) -> bool {
    BUSHY_ALLOWED.with(|c| c.replace(allowed))
}

/// Install (`Some`) or remove (`None`) a hook called at every poll of a
/// plan search on this thread, before the stop condition is read. For
/// tests: a hook that raises the query's cancel flag shows the search sees
/// a cancel at its next poll (review query#13).
#[doc(hidden)]
pub fn set_poll_hook_on_this_thread(hook: Option<Box<dyn FnMut()>>) {
    POLL_HOOK.with(|h| *h.borrow_mut() = hook);
}

/// Count `n` work units on this thread.
fn count_work(n: u64) {
    let _ = WORK.try_with(|w| w.set(w.get().saturating_add(n)));
}

/// The work account of one 2-opt search: [`OPT_WORK_BUDGET`] units, a poll
/// of `stop` every [`POLL_EVERY`].
struct Search<'s> {
    stop: &'s dyn Fn() -> bool,
    spent: u64,
    next_poll: u64,
    stopped: bool,
}

impl<'s> Search<'s> {
    fn new(stop: &'s dyn Fn() -> bool) -> Search<'s> {
        Search {
            stop,
            spent: 0,
            next_poll: POLL_EVERY,
            stopped: false,
        }
    }

    /// Charge `n` units; `false` once the search must end (out of budget,
    /// or stopped at a poll).
    fn charge(&mut self, n: u64) -> bool {
        self.spent = self.spent.saturating_add(n);
        count_work(n);
        if self.spent >= self.next_poll {
            self.next_poll = self.spent.saturating_add(POLL_EVERY);
            let _ = POLL_HOOK.try_with(|h| {
                if let Some(hook) = h.borrow_mut().as_mut() {
                    hook();
                }
            });
            if (self.stop)() {
                self.stopped = true;
            }
        }
        !self.stopped && self.spent < OPT_WORK_BUDGET
    }
}

/// Compute the execution plan for a BGP: optimal left-deep order, plus a bushy
/// binary-join tree when one is strictly cheaper. Never stops early (see
/// [`optimize_until`]).
#[cfg(test)]
pub(crate) fn optimize(
    plans: &[PatPlan],
    store: &impl TripleSource,
    candidates: &Candidates,
    num_vars: usize,
) -> ExecPlan {
    optimize_until(plans, store, candidates, num_vars, &|| false)
}

/// [`optimize`] for a query that may be stopped: the plan search of a large
/// BGP polls `stop` (the evaluator's cancel check) and, once it answers
/// `true`, returns the best plan found so far — a valid order the caller
/// should not cache (it reports the cancel instead of running it).
pub(crate) fn optimize_until(
    plans: &[PatPlan],
    store: &impl TripleSource,
    candidates: &Candidates,
    num_vars: usize,
    stop: &dyn Fn() -> bool,
) -> ExecPlan {
    let n = plans.len();
    if n <= 1 {
        return ExecPlan {
            order: (0..n).collect(),
            tree: None,
        };
    }
    // The DP indexes patterns by bit and variables by bit; both must fit a mask.
    // Beyond that the exact DP is infeasible: build a greedy order and then
    // refine it with bounded 2-opt local search (cheaper, better-than-greedy
    // plans for 15–30-pattern conjunctive queries) instead of using raw greedy.
    if n > LEFTDEEP_DP_LIMIT || num_vars > 64 {
        return ExecPlan {
            order: greedy_2opt_until(plans, store, candidates, num_vars, stop),
            tree: None,
        };
    }

    let model = CostModel::build(plans, store, candidates);

    // Optimal left-deep order (the executor's default path) and its cost.
    let (order, left_deep_cost) = dp_left_deep(n, &model);
    count_work((n as u64) << n);

    // Bushy DP: only worthwhile for ≥4 patterns and within the 3ⁿ budget. Use a
    // binary-join tree only when it is genuinely bushy *and* strictly cheaper.
    let bushy_allowed = BUSHY_ALLOWED.try_with(Cell::get).unwrap_or(true);
    let tree = if bushy_allowed && (4..=BUSHY_DP_LIMIT).contains(&n) {
        let (cost_bushy, tree) = dp_bushy(n, &model);
        count_work(3u64.pow(n as u32));
        if !tree.is_left_deep() && cost_bushy < left_deep_cost {
            let _ = BUSHY_CHOSEN.try_with(|c| c.set(c.get() + 1));
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

// --- large-BGP path: greedy + 2-opt -----------------------------------------

/// Maximum 2-opt refinement passes over a greedy order. Each accepted swap
/// strictly lowers the estimated cost, so this only bounds pathological cases.
const MAX_2OPT_PASSES: usize = 16;

/// Per-pattern cost inputs without the `2ⁿ` subset memo, so the cost of an
/// arbitrary left-deep order can be evaluated in `O(n · vars)` for BGPs far
/// larger than the exact DP's reach. Mirrors [`CostModel`]'s statistics, but
/// keeps only per-pattern data.
struct CostModelLite {
    /// `card[i]` — estimated number of triples matching pattern `i`.
    card: Vec<f64>,
    /// `vars[i]` — the distinct variable indices appearing in pattern `i`.
    vars: Vec<Vec<usize>>,
    /// `ndv[i][v]` — distinct values variable `v` takes in pattern `i`.
    ndv: Vec<HashMap<usize, f64>>,
    /// One past the largest variable index (sizes a [`Prefix`]).
    num_vars: usize,
}

impl CostModelLite {
    fn build(
        plans: &[PatPlan],
        store: &impl TripleSource,
        candidates: &Candidates,
        num_vars: usize,
    ) -> CostModelLite {
        let n = plans.len();
        let mut card = vec![1.0; n];
        let mut vars = vec![Vec::new(); n];
        let mut ndv = vec![HashMap::new(); n];
        for (i, plan) in plans.iter().enumerate() {
            card[i] = pattern_card(plan, store);
            vars[i] = plan_vars(plan);
            ndv[i] = build_ndv(plan, store, candidates);
        }
        CostModelLite {
            card,
            vars,
            ndv,
            num_vars,
        }
    }

    /// Estimated total cost of evaluating the patterns in `order` as a left-deep
    /// pipeline — the same System-R recurrence the DP minimises
    /// (`cost = card(first) + Σ join_card(prefix, next)`), evaluated directly so
    /// any permutation can be scored without the subset table.
    #[cfg(test)]
    fn order_cost(&self, order: &[usize]) -> f64 {
        self.order_cost_in(order, &mut Prefix::new(self.num_vars))
    }

    /// [`order_cost`](Self::order_cost) with reusable prefix statistics
    /// (no allocation per call: the 2-opt search scores many orders).
    fn order_cost_in(&self, order: &[usize], prefix: &mut Prefix) -> f64 {
        let Some((&first, rest)) = order.split_first() else {
            return 0.0;
        };
        // Min finite NDV per already-covered variable across the placed prefix.
        prefix.reset();
        let mut run_card = self.card[first];
        let mut cost = run_card;
        self.absorb(first, prefix);

        for &i in rest {
            let mut denom = 1.0;
            let mut shared = false;
            for &v in &self.vars[i] {
                if prefix.covers(v) {
                    shared = true;
                    let nv_run = prefix.ndv(v).unwrap_or(1.0);
                    let nv_pat = self.ndv[i].get(&v).copied().unwrap_or(1.0);
                    denom *= nv_run.max(nv_pat).max(1.0);
                }
            }
            let jc = if shared {
                (run_card * self.card[i] / denom).max(1.0)
            } else {
                run_card * self.card[i] // disconnected ⇒ cross product
            };
            cost += jc;
            run_card = jc;
            self.absorb(i, prefix);
        }
        cost
    }

    /// Fold pattern `i`'s variables and NDVs into the running prefix statistics.
    fn absorb(&self, i: usize, prefix: &mut Prefix) {
        for &v in &self.vars[i] {
            prefix.cover(v, self.ndv[i].get(&v).copied());
        }
    }
}

/// The running statistics of an order's placed prefix for
/// [`CostModelLite::order_cost_in`]: which variables it covers and the
/// least NDV it gives each. Indexed by variable, reset in O(1) by bumping
/// an epoch instead of clearing.
struct Prefix {
    /// `epoch` when the variable is covered by the current prefix.
    seen: Vec<u32>,
    /// The least NDV of a covered variable (`NaN`: none known).
    ndv: Vec<f64>,
    epoch: u32,
}

impl Prefix {
    fn new(num_vars: usize) -> Prefix {
        Prefix {
            seen: vec![0; num_vars],
            ndv: vec![f64::NAN; num_vars],
            epoch: 0,
        }
    }

    fn reset(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        if self.epoch == 0 {
            // Wrapped: stale marks could read as current.
            self.seen.fill(0);
            self.epoch = 1;
        }
    }

    fn covers(&self, v: usize) -> bool {
        self.seen.get(v) == Some(&self.epoch)
    }

    fn ndv(&self, v: usize) -> Option<f64> {
        if self.covers(v) {
            Some(self.ndv[v]).filter(|d| !d.is_nan())
        } else {
            None
        }
    }

    fn cover(&mut self, v: usize, d: Option<f64>) {
        if v >= self.seen.len() {
            self.seen.resize(v + 1, 0);
            self.ndv.resize(v + 1, f64::NAN);
        }
        if self.seen[v] != self.epoch {
            self.seen[v] = self.epoch;
            self.ndv[v] = f64::NAN;
        }
        if let Some(d) = d {
            // The first known NDV, then the least one.
            if self.ndv[v].is_nan() || d < self.ndv[v] {
                self.ndv[v] = d;
            }
        }
    }
}

/// Build a greedy left-deep order (gStore's `PlanGenerator` heuristic) and then
/// refine it with bounded 2-opt local search under the System-R cost model: try
/// swapping each pair of positions and keep the single swap that most reduces
/// the estimated cost, repeating until no swap helps. The result is never worse
/// than the greedy seed and typically markedly better for 15–30-pattern BGPs.
/// Never stops early (see [`greedy_2opt_until`]).
#[cfg(test)]
pub(crate) fn greedy_2opt(
    plans: &[PatPlan],
    store: &impl TripleSource,
    candidates: &Candidates,
    num_vars: usize,
) -> Vec<usize> {
    greedy_2opt_until(plans, store, candidates, num_vars, &|| false)
}

/// [`greedy_2opt`] within [`OPT_WORK_BUDGET`], polling `stop` every
/// [`POLL_EVERY`] work units: out of budget or stopped, it ends with the
/// best order found so far (a pass cut short still applies the best swap it
/// found). Above [`MAX_2OPT_PATTERNS`] patterns the greedy order is
/// returned as is (review query#13).
pub(crate) fn greedy_2opt_until(
    plans: &[PatPlan],
    store: &impl TripleSource,
    candidates: &Candidates,
    num_vars: usize,
    stop: &dyn Fn() -> bool,
) -> Vec<usize> {
    let mut order = super::planner::plan(plans, store, candidates, num_vars);
    if order.len() < 3 || order.len() > MAX_2OPT_PATTERNS {
        return order; // nothing for a pairwise swap to improve, or too large
    }
    let model = CostModelLite::build(plans, store, candidates, num_vars);
    let mut prefix = Prefix::new(model.num_vars);
    let mut search = Search::new(stop);
    let n = order.len() as u64;
    let mut best_cost = model.order_cost_in(&order, &mut prefix);
    let mut going = search.charge(n);

    for _ in 0..MAX_2OPT_PASSES {
        if !going {
            break;
        }
        let mut best_swap: Option<(usize, usize)> = None;
        let mut best_after = best_cost;
        'pass: for a in 0..order.len() {
            for b in (a + 1)..order.len() {
                order.swap(a, b);
                let c = model.order_cost_in(&order, &mut prefix);
                order.swap(a, b);
                if c < best_after {
                    best_after = c;
                    best_swap = Some((a, b));
                }
                going = search.charge(n);
                if !going {
                    break 'pass;
                }
            }
        }
        match best_swap {
            Some((a, b)) => {
                order.swap(a, b);
                best_cost = best_after;
            }
            None => break,
        }
    }
    order
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
/// index range the executor would scan for it (counted, not materialized:
/// contract C1).
fn pattern_card(plan: &PatPlan, store: &impl TripleSource) -> f64 {
    use Slot::{Const, Var};
    let c = match (plan.s, plan.p, plan.o) {
        (Const(_), Const(_), Const(_)) => 1u64,
        (Const(s), Const(p), Var(_)) => store.count_o_by_sp(s, p),
        (Var(_), Const(p), Const(o)) => store.count_s_by_po(p, o),
        (Const(s), Var(_), Const(o)) => store.count_p_by_so(s, o),
        (Const(s), Var(_), Var(_)) => store.count_po_by_s(s),
        (Var(_), Const(p), Var(_)) => store.count_so_by_p(p),
        (Var(_), Var(_), Const(o)) => store.count_ps_by_o(o),
        (Var(_), Var(_), Var(_)) => store.triple_count(),
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

    // Counted, not materialized (contract C1); every value is the one the
    // list-based estimate gave, so plans are unchanged.
    if let Var(v) = plan.s {
        let d = match (plan.p, plan.o) {
            (Const(p), Const(o)) => store.count_s_by_po(p, o) as usize,
            (Const(p), Var(_)) => store.pred_distinct_subj(p),
            // FIXME: meant as the distinct subjects of `o`, but this counts the
            // distinct *predicates* of `o` (the first components of `ps_by_o`).
            // Kept as is: changing it would change plans.
            (Var(_), Const(o)) => store.distinct_p_of_o(o) as usize,
            (Var(_), Var(_)) => store.distinct_subjects(),
        };
        put(v, d);
    }
    if let Var(v) = plan.o {
        let d = match (plan.s, plan.p) {
            (Const(s), Const(p)) => store.count_o_by_sp(s, p) as usize,
            (Var(_), Const(p)) => store.pred_distinct_obj(p),
            (Const(s), Var(_)) => store.count_po_by_s(s) as usize, // objects of s (upper bound)
            (Var(_), Var(_)) => store.distinct_objects(),
        };
        put(v, d);
    }
    if let Var(v) = plan.p {
        let d = match (plan.s, plan.o) {
            (Const(s), Const(o)) => store.count_p_by_so(s, o) as usize,
            (Const(s), Var(_)) => store.distinct_p_of_s(s) as usize, // distinct preds of s
            (Var(_), Const(o)) => store.distinct_p_of_o(o) as usize, // distinct preds of o
            (Var(_), Var(_)) => store.num_predicates(),
        };
        put(v, d);
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::IdTriple;
    use crate::query::candidates;
    use crate::store::TripleStore;

    fn pp(s: Slot, p: Slot, o: Slot) -> PatPlan {
        PatPlan { s, p, o }
    }

    /// Two stars (?a around x, ?b around y) bridged by a ?x→?y edge.
    fn bridge_store() -> TripleStore {
        // preds: ta=0, tb=1, bridge=2
        let mut s = TripleStore::new();
        let mut t = Vec::new();
        for a in 0..50u32 {
            t.push(IdTriple::new(a, 0, 1000));
        }
        for b in 100..150u32 {
            t.push(IdTriple::new(b, 1, 2000));
        }
        t.push(IdTriple::new(10, 2, 110));
        s.bulk_load(t);
        s
    }

    #[test]
    fn left_deep_order_is_a_permutation() {
        let s = bridge_store();
        let plans = vec![
            pp(Slot::Var(0), Slot::Const(0), Slot::Const(1000)),
            pp(Slot::Var(1), Slot::Const(1), Slot::Const(2000)),
            pp(Slot::Var(0), Slot::Const(2), Slot::Var(1)),
        ];
        let c = candidates::generate(&plans, &s);
        let plan = optimize(&plans, &s, &c, 2);
        let mut o = plan.order.clone();
        o.sort_unstable();
        assert_eq!(o, vec![0, 1, 2]);
    }

    #[test]
    fn single_pattern_is_trivial() {
        let s = bridge_store();
        let plans = vec![pp(Slot::Var(0), Slot::Const(0), Slot::Const(1000))];
        let c = candidates::generate(&plans, &s);
        let plan = optimize(&plans, &s, &c, 1);
        assert_eq!(plan.order, vec![0]);
        assert!(plan.tree.is_none());
    }

    #[test]
    fn bushy_tree_covers_all_patterns_once() {
        // Two 2-stars joined by a bridge: 0,1 around ?x ; 2,3 around ?y ; 4 bridges.
        let mut s = TripleStore::new();
        let mut t = Vec::new();
        for x in 0..40u32 {
            t.push(IdTriple::new(x, 0, 900));
            t.push(IdTriple::new(x, 1, 901));
        }
        for y in 100..140u32 {
            t.push(IdTriple::new(y, 2, 902));
            t.push(IdTriple::new(y, 3, 903));
        }
        t.push(IdTriple::new(5, 4, 105));
        s.bulk_load(t);

        // vars: x=0, y=1
        let plans = vec![
            pp(Slot::Var(0), Slot::Const(0), Slot::Const(900)),
            pp(Slot::Var(0), Slot::Const(1), Slot::Const(901)),
            pp(Slot::Var(1), Slot::Const(2), Slot::Const(902)),
            pp(Slot::Var(1), Slot::Const(3), Slot::Const(903)),
            pp(Slot::Var(0), Slot::Const(4), Slot::Var(1)),
        ];
        let c = candidates::generate(&plans, &s);
        let plan = optimize(&plans, &s, &c, 2);
        // review tests-b#14: the check used to sit in `if let Some(tree)`,
        // passing silently when no bushy tree was chosen; this bow-tie is
        // the shape the bushy DP exists for, so the tree is required.
        let tree = plan.tree.as_ref().expect("a bushy tree for the bow-tie");
        let mut leaves = Vec::new();
        collect_leaves(tree, &mut leaves);
        leaves.sort_unstable();
        assert_eq!(leaves, vec![0, 1, 2, 3, 4]);
        let mut o = plan.order.clone();
        o.sort_unstable();
        assert_eq!(o, vec![0, 1, 2, 3, 4]);
    }

    fn collect_leaves(t: &JoinTree, out: &mut Vec<usize>) {
        match t {
            JoinTree::Leaf(i) => out.push(*i),
            JoinTree::Join(l, r) => {
                collect_leaves(l, out);
                collect_leaves(r, out);
            }
        }
    }

    #[test]
    fn join_card_shrinks_with_selective_join() {
        let s = bridge_store();
        let plans = vec![
            pp(Slot::Var(0), Slot::Const(0), Slot::Const(1000)),
            pp(Slot::Var(0), Slot::Const(2), Slot::Var(1)),
        ];
        let c = candidates::generate(&plans, &s);
        let model = CostModel::build(&plans, &s, &c);
        // joining the 50-row ta-star with the 1-row bridge on ?x yields ≤ 51.
        let jc = model.join_card(0b01, 0b10, model.card[0], model.card[1]);
        assert!(jc <= model.card[0].max(model.card[1]) + 1.0);
    }

    #[test]
    fn judge_picks_nested_for_tiny_and_hash_for_large() {
        use super::{judge, JoinMethod};
        assert_eq!(judge(1, 1000), JoinMethod::NestedLoop);
        assert_eq!(judge(1000, 1), JoinMethod::NestedLoop);
        assert_eq!(judge(2, 2), JoinMethod::NestedLoop);
        assert_eq!(judge(60, 60), JoinMethod::Hash);
        assert_eq!(judge(5, 1000), JoinMethod::Hash);
    }

    /// A 20-pattern chain BGP: greedy+2-opt yields a valid permutation whose
    /// estimated cost never exceeds the greedy seed's (and usually beats it).
    #[test]
    fn greedy_2opt_does_not_regress_large_bgp() {
        // Chain ?v0 -p0-> ?v1 -p1-> ... -p19-> ?v20 (20 patterns, 21 vars), with
        // widely varying predicate cardinalities so ordering matters.
        let mut s = TripleStore::new();
        let mut t = Vec::new();
        // Each predicate i connects a band of subjects to a band of objects; the
        // fan-out per predicate varies to create non-trivial join costs.
        let bases: [u32; 20] = [
            3, 50, 2, 80, 5, 120, 4, 30, 7, 200, 6, 40, 9, 90, 8, 25, 11, 150, 10, 60,
        ];
        for (i, &width) in bases.iter().enumerate() {
            let pred = i as u32;
            for j in 0..width {
                // node ids namespaced by level so the chain links up
                let sub = (i as u32) * 1_000 + j;
                let obj = (i as u32 + 1) * 1_000 + (j % 3); // narrow join fan-in
                t.push(IdTriple::new(sub, pred, obj));
            }
        }
        s.bulk_load(t);

        // Build the 20 chain patterns: pattern i = (?v_i, p_i, ?v_{i+1}).
        let plans: Vec<PatPlan> = (0..20usize)
            .map(|i| pp(Slot::Var(i), Slot::Const(i as u32), Slot::Var(i + 1)))
            .collect();
        let c = candidates::generate(&plans, &s);
        let num_vars = 21;

        let greedy = super::super::planner::plan(&plans, &s, &c, num_vars);
        let refined = super::greedy_2opt(&plans, &s, &c, num_vars);

        // Both are permutations of the 20 patterns.
        let mut g = greedy.clone();
        g.sort_unstable();
        assert_eq!(g, (0..20).collect::<Vec<_>>());
        let mut r = refined.clone();
        r.sort_unstable();
        assert_eq!(r, (0..20).collect::<Vec<_>>());

        // 2-opt never produces a worse plan than its greedy seed.
        let model = super::CostModelLite::build(&plans, &s, &c, num_vars);
        let cost_greedy = model.order_cost(&greedy);
        let cost_refined = model.order_cost(&refined);
        assert!(
            cost_refined <= cost_greedy + 1e-6,
            "2-opt regressed: greedy={cost_greedy}, refined={cost_refined}"
        );

        // Determinism: re-running gives the same order.
        let refined2 = super::greedy_2opt(&plans, &s, &c, num_vars);
        assert_eq!(refined, refined2);
    }

    /// 2-opt strictly improves a deliberately bad greedy seed: scoring three
    /// patterns where swapping a late selective pattern earlier lowers cost.
    #[test]
    fn order_cost_rewards_selective_first() {
        let s = bridge_store();
        let plans = vec![
            pp(Slot::Var(0), Slot::Const(0), Slot::Const(1000)), // 50 rows
            pp(Slot::Var(1), Slot::Const(1), Slot::Const(2000)), // 50 rows
            pp(Slot::Var(0), Slot::Const(2), Slot::Var(1)),      // 1-row bridge
        ];
        let c = candidates::generate(&plans, &s);
        let model = super::CostModelLite::build(&plans, &s, &c, 2);
        // Leading with the selective bridge then expanding is cheaper than two
        // big independent stars first (whose join is a near cross-product).
        let cost_bridge_first = model.order_cost(&[2, 0, 1]);
        let cost_stars_first = model.order_cost(&[0, 1, 2]);
        assert!(cost_bridge_first < cost_stars_first);
    }

    #[test]
    fn is_left_deep_detects_shape() {
        let ld = JoinTree::Join(
            Box::new(JoinTree::Join(
                Box::new(JoinTree::Leaf(0)),
                Box::new(JoinTree::Leaf(1)),
            )),
            Box::new(JoinTree::Leaf(2)),
        );
        assert!(ld.is_left_deep());
        let bushy = JoinTree::Join(
            Box::new(JoinTree::Join(
                Box::new(JoinTree::Leaf(0)),
                Box::new(JoinTree::Leaf(1)),
            )),
            Box::new(JoinTree::Join(
                Box::new(JoinTree::Leaf(2)),
                Box::new(JoinTree::Leaf(3)),
            )),
        );
        assert!(!bushy.is_left_deep());
    }

    /// The order cost before review query#13 (fresh maps per call): the
    /// reference the allocation-free scoring must agree with exactly, so
    /// the plans stay the same.
    fn reference_order_cost(model: &CostModelLite, order: &[usize]) -> f64 {
        if order.is_empty() {
            return 0.0;
        }
        let mut run_ndv: HashMap<usize, f64> = HashMap::new();
        let mut covered: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let absorb = |i: usize,
                      covered: &mut std::collections::HashSet<usize>,
                      run_ndv: &mut HashMap<usize, f64>| {
            for &v in &model.vars[i] {
                covered.insert(v);
                if let Some(&d) = model.ndv[i].get(&v) {
                    let e = run_ndv.entry(v).or_insert(d);
                    if d < *e {
                        *e = d;
                    }
                }
            }
        };
        let first = order[0];
        let mut run_card = model.card[first];
        let mut cost = run_card;
        absorb(first, &mut covered, &mut run_ndv);
        for &i in &order[1..] {
            let mut denom = 1.0;
            let mut shared = false;
            for &v in &model.vars[i] {
                if covered.contains(&v) {
                    shared = true;
                    let nv_run = run_ndv.get(&v).copied().unwrap_or(1.0);
                    let nv_pat = model.ndv[i].get(&v).copied().unwrap_or(1.0);
                    denom *= nv_run.max(nv_pat).max(1.0);
                }
            }
            let jc = if shared {
                (run_card * model.card[i] / denom).max(1.0)
            } else {
                run_card * model.card[i]
            };
            cost += jc;
            run_card = jc;
            absorb(i, &mut covered, &mut run_ndv);
        }
        cost
    }

    /// A chain of `n` patterns `?v_i p_i ?v_{i+1}` over predicates of
    /// varying fan-out, and its candidate sets.
    fn chain(n: usize) -> (TripleStore, Vec<PatPlan>) {
        let mut s = TripleStore::new();
        let mut t = Vec::new();
        for i in 0..n as u32 {
            for j in 0..(3 + (i * 37) % 50) {
                t.push(IdTriple::new(i * 1_000 + j, i, (i + 1) * 1_000 + (j % 3)));
            }
        }
        s.bulk_load(t);
        let plans = (0..n)
            .map(|i| pp(Slot::Var(i), Slot::Const(i as u32), Slot::Var(i + 1)))
            .collect();
        (s, plans)
    }

    /// A star of `n` patterns `?s p_i ?o_i` over 20 subjects.
    fn star(n: usize) -> (TripleStore, Vec<PatPlan>) {
        let mut s = TripleStore::new();
        let mut t = Vec::new();
        for subj in 0..20u32 {
            for p in 0..n as u32 {
                t.push(IdTriple::new(
                    subj,
                    p,
                    100_000 + subj * 1_000 + p % (1 + subj),
                ));
            }
        }
        s.bulk_load(t);
        let plans = (0..n)
            .map(|i| pp(Slot::Var(0), Slot::Const(i as u32), Slot::Var(i + 1)))
            .collect();
        (s, plans)
    }

    #[test]
    fn allocation_free_order_cost_matches_the_reference() {
        // review query#13: the reused prefix statistics score every order
        // exactly as the per-call maps did (so no plan changes).
        for (s, plans) in [chain(20), star(30)] {
            let num_vars = plans.len() + 1;
            let c = candidates::generate(&plans, &s);
            let model = CostModelLite::build(&plans, &s, &c, num_vars);
            let mut prefix = Prefix::new(num_vars);
            let mut order: Vec<usize> = (0..plans.len()).collect();
            let mut x = 0x9E37_79B9_7F4A_7C15u64;
            for _ in 0..200 {
                // A deterministic shuffle (xorshift).
                for i in (1..order.len()).rev() {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    order.swap(i, (x % (i as u64 + 1)) as usize);
                }
                let want = reference_order_cost(&model, &order);
                assert_eq!(
                    model.order_cost_in(&order, &mut prefix).to_bits(),
                    want.to_bits()
                );
                assert_eq!(model.order_cost(&order).to_bits(), want.to_bits());
            }
        }
    }

    /// The work one `greedy_2opt_until` call spends (this thread's counter).
    fn work_of(f: impl FnOnce()) -> u64 {
        let before = work_on_this_thread();
        f();
        work_on_this_thread() - before
    }

    #[test]
    fn the_2opt_search_stays_within_its_work_budget() {
        // review query#13: unbounded, 16 passes of n²/2 swaps scored in
        // O(n) each — 16·780·40 ≈ 0.5M units at 40 patterns but 64M at
        // 200 and 512M at 400.
        for n in [40, 100, 200, 256] {
            let (s, plans) = star(n);
            let c = candidates::generate(&plans, &s);
            let mut order = Vec::new();
            let w = work_of(|| order = greedy_2opt_until(&plans, &s, &c, n + 1, &|| false));
            assert!(
                w <= OPT_WORK_BUDGET + n as u64,
                "{n} patterns: {w} work units, over the budget {OPT_WORK_BUDGET}"
            );
            let mut sorted = order.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, (0..n).collect::<Vec<_>>(), "a permutation");
        }
        // Beyond MAX_2OPT_PATTERNS the greedy order is used as is.
        let (s, plans) = star(MAX_2OPT_PATTERNS + 1);
        let c = candidates::generate(&plans, &s);
        let w = work_of(|| {
            greedy_2opt_until(&plans, &s, &c, plans.len() + 1, &|| false);
        });
        assert_eq!(w, 0);
    }

    #[test]
    fn a_stop_ends_the_2opt_search_at_its_next_poll() {
        // review query#13: the search polls the stop condition every
        // POLL_EVERY units and ends there with a valid order.
        let n = 200;
        let (s, plans) = star(n);
        let c = candidates::generate(&plans, &s);
        let polls = Cell::new(0u64);
        let stop = || {
            polls.set(polls.get() + 1);
            true
        };
        let mut order = Vec::new();
        let w = work_of(|| order = greedy_2opt_until(&plans, &s, &c, n + 1, &stop));
        assert_eq!(polls.get(), 1, "stopped at the first poll");
        assert!(w <= POLL_EVERY + n as u64, "{w} units after the stop");
        let mut sorted = order;
        sorted.sort_unstable();
        assert_eq!(sorted, (0..n).collect::<Vec<_>>());
        // A stop that never comes: the whole budget may be spent.
        let w = work_of(|| {
            greedy_2opt_until(&plans, &s, &c, n + 1, &|| false);
        });
        assert!(w > POLL_EVERY + n as u64, "only {w} units without a stop");
    }

    #[test]
    fn the_bushy_switch_and_counter_follow_the_plans() {
        // review tests-b#14: the hooks dt_optimizer uses — the bow-tie of
        // `bushy_tree_covers_all_patterns_once` gets a bushy tree, and with
        // bushy plans forbidden on the thread a left-deep one.
        let mut s = TripleStore::new();
        let mut t = Vec::new();
        for x in 0..40u32 {
            t.push(IdTriple::new(x, 0, 900));
            t.push(IdTriple::new(x, 1, 901));
        }
        for y in 100..140u32 {
            t.push(IdTriple::new(y, 2, 902));
            t.push(IdTriple::new(y, 3, 903));
        }
        t.push(IdTriple::new(5, 4, 105));
        s.bulk_load(t);
        let plans = vec![
            pp(Slot::Var(0), Slot::Const(0), Slot::Const(900)),
            pp(Slot::Var(0), Slot::Const(1), Slot::Const(901)),
            pp(Slot::Var(1), Slot::Const(2), Slot::Const(902)),
            pp(Slot::Var(1), Slot::Const(3), Slot::Const(903)),
            pp(Slot::Var(0), Slot::Const(4), Slot::Var(1)),
        ];
        let c = candidates::generate(&plans, &s);
        let before = bushy_plans_on_this_thread();
        let plan = optimize(&plans, &s, &c, 2);
        assert!(plan.tree.as_ref().is_some_and(|t| !t.is_left_deep()));
        assert_eq!(bushy_plans_on_this_thread(), before + 1);
        assert!(set_bushy_plans_on_this_thread(false));
        let plan = optimize(&plans, &s, &c, 2);
        assert!(!set_bushy_plans_on_this_thread(true));
        assert!(plan.tree.is_none());
        assert_eq!(bushy_plans_on_this_thread(), before + 1);
    }
}
