//! Cost-based join ordering for basic graph patterns.
//!
//! Corresponds to gStore's `Optimizer` / `PlanGenerator` / `PlanTree`. Given a
//! BGP compiled to [`PatPlan`]s and the [`TripleStore`] statistics, it picks a
//! left-deep join order minimizing estimated intermediate-result size, via a
//! Selinger-style dynamic program over pattern subsets.
//!
//! Estimation model (textbook, independence + containment assumptions):
//! * a pattern's **base cardinality** is read exactly from the index where its
//!   constants allow (e.g. `?s p o` → `|s_by_po(p,o)|`), else from predicate
//!   statistics (`pred_card`/`pred_distinct_subj`/`pred_distinct_obj`);
//! * joining a pattern that shares variable `v` divides by `domain(v)`, the
//!   estimated number of distinct values of `v`.
//!
//! For BGPs with more patterns than [`MAX_DP_PATTERNS`] (DP is `O(2^n · n)`) we
//! fall back to a connected, smallest-first greedy order.

use crate::store::TripleStore;

use super::candidates::Candidates;
use super::engine::{PatPlan, Slot};

/// Above this many patterns, use the greedy fallback instead of exact DP.
const MAX_DP_PATTERNS: usize = 16;

/// Penalty (in estimated rows) added when a join step is a cartesian product,
/// to push unavoidable cross-products to the end of the plan.
const CARTESIAN_PENALTY: f64 = 1e12;

/// Compute a left-deep evaluation order for `plans` (indices into `plans`),
/// using exact candidate sizes where available (gStore's `var_to_num`).
pub(crate) fn order_bgp(
    plans: &[PatPlan],
    store: &TripleStore,
    candidates: &Candidates,
) -> Vec<usize> {
    let n = plans.len();
    if n <= 1 {
        return (0..n).collect();
    }

    // Per-pattern variable bitmask; bail to greedy if a var index exceeds 63.
    let num_vars = max_var_index(plans).map_or(0, |m| m + 1);
    if n > MAX_DP_PATTERNS || num_vars > 64 {
        return greedy_order(plans, store, candidates);
    }
    let varset: Vec<u64> = plans.iter().map(var_mask).collect();

    let base: Vec<f64> = plans
        .iter()
        .map(|p| base_card_eff(p, store, candidates) as f64)
        .collect();
    let domain = domain_estimates(plans, store, num_vars, candidates);

    dp_order(&base, &varset, &domain)
}

/// Effective base cardinality: the index estimate, capped by the smallest
/// candidate set among the pattern's variables (a pattern can't yield more rows
/// than its most-constrained variable allows).
fn base_card_eff(plan: &PatPlan, store: &TripleStore, candidates: &Candidates) -> u64 {
    let mut card = base_card(plan, store);
    for slot in [&plan.s, &plan.p, &plan.o] {
        if let Slot::Var(v) = slot {
            if let Some(c) = candidates.get(v) {
                card = card.min(c.len() as u64);
            }
        }
    }
    card.max(1)
}

/// Selinger DP: `cost[S]` = min total intermediate size to evaluate set `S`.
fn dp_order(base: &[f64], varset: &[u64], domain: &[f64]) -> Vec<usize> {
    let n = base.len();
    let full = (1usize << n) - 1;
    let mut cost = vec![f64::INFINITY; full + 1];
    let mut card = vec![0f64; full + 1];
    let mut back = vec![usize::MAX; full + 1]; // last pattern added to reach S
    let mut vmask = vec![0u64; full + 1]; // union of variable masks in S

    for i in 0..n {
        let s = 1usize << i;
        cost[s] = base[i];
        card[s] = base[i];
        back[s] = i;
        vmask[s] = varset[i];
    }

    for s in 1..=full {
        if s.count_ones() < 2 {
            continue;
        }
        let mut bits = s;
        while bits != 0 {
            let i = bits.trailing_zeros() as usize;
            bits &= bits - 1;
            let prev = s ^ (1usize << i);
            if cost[prev].is_infinite() {
                continue;
            }
            let prev_vars = vmask[prev];
            let shared = varset[i] & prev_vars;
            // Divide base card by the domain of each shared (join) variable.
            let mut divisor = 1.0f64;
            let mut sb = shared;
            while sb != 0 {
                let v = sb.trailing_zeros() as usize;
                sb &= sb - 1;
                divisor *= domain[v].max(1.0);
            }
            let added = (base[i] / divisor).max(1.0);
            let new_card = (card[prev] * added).max(1.0);
            let connected = shared != 0 || varset[i] == 0;
            let mut c = cost[prev] + new_card;
            if !connected {
                c += CARTESIAN_PENALTY;
            }
            if c < cost[s] {
                cost[s] = c;
                card[s] = new_card;
                back[s] = i;
                vmask[s] = prev_vars | varset[i];
            }
        }
    }

    // Reconstruct the order by following back-pointers from the full set.
    let mut order = Vec::with_capacity(n);
    let mut s = full;
    while s != 0 {
        let i = back[s];
        debug_assert_ne!(i, usize::MAX);
        order.push(i);
        s ^= 1usize << i;
    }
    order.reverse();
    order
}

/// Estimate, per variable, its number of distinct values (smaller = more
/// selective). Taken as the minimum over the variable's occurrences.
fn domain_estimates(
    plans: &[PatPlan],
    store: &TripleStore,
    num_vars: usize,
    candidates: &Candidates,
) -> Vec<f64> {
    let big = (store.triple_count() as f64).max(1.0);
    let mut dom = vec![big; num_vars];
    let mut see = |v: usize, est: f64| {
        if est > 0.0 && est < dom[v] {
            dom[v] = est;
        }
    };
    let n_subj = store.distinct_subjects().max(1) as f64;
    let n_obj = store.distinct_objects().max(1) as f64;
    let n_pred = store.num_predicates().max(1) as f64;

    for p in plans {
        let pred_const = p.p.const_id();
        if let Some(v) = p.s.var() {
            let est = match pred_const {
                Some(pred) => store.pred_distinct_subj(pred) as f64,
                None => n_subj,
            };
            see(v, est);
        }
        if let Some(v) = p.o.var() {
            let est = match pred_const {
                Some(pred) => store.pred_distinct_obj(pred) as f64,
                None => n_obj,
            };
            see(v, est);
        }
        if let Some(v) = p.p.var() {
            see(v, n_pred);
        }
    }
    // An exact candidate set is the tightest distinct-value bound there is.
    for (&v, c) in candidates {
        if v < num_vars {
            see(v, c.len() as f64);
        }
    }
    dom
}

/// Exact-or-estimated base cardinality of a single pattern.
fn base_card(plan: &PatPlan, store: &TripleStore) -> u64 {
    let s = plan.s.const_id();
    let p = plan.p.const_id();
    let o = plan.o.const_id();
    match (s, p, o) {
        (Some(_), Some(_), Some(_)) => 1,
        (Some(s), Some(p), None) => store.o_by_sp(s, p).len() as u64,
        (None, Some(p), Some(o)) => store.s_by_po(p, o).len() as u64,
        (Some(s), None, Some(o)) => store.p_by_so(s, o).len() as u64,
        (Some(s), None, None) => store.po_by_s(s).len() as u64,
        (None, Some(p), None) => store.so_by_p(p).len() as u64,
        (None, None, Some(o)) => store.ps_by_o(o).len() as u64,
        (None, None, None) => store.triple_count(),
    }
    .max(1)
}

/// Connected, smallest-base-card-first greedy order (fallback for large BGPs).
fn greedy_order(plans: &[PatPlan], store: &TripleStore, candidates: &Candidates) -> Vec<usize> {
    let n = plans.len();
    let mut remaining: Vec<usize> = (0..n).collect();
    let mut order = Vec::with_capacity(n);
    let mut bound = std::collections::HashSet::new();

    while !remaining.is_empty() {
        let first = order.is_empty();
        let pos = remaining
            .iter()
            .enumerate()
            .min_by_key(|&(_, &pi)| {
                let plan = &plans[pi];
                let connected = first || shares_bound_var(plan, &bound);
                let known = known_count(plan, &bound);
                (
                    !connected as u8,
                    std::cmp::Reverse(known),
                    base_card_eff(plan, store, candidates),
                )
            })
            .map(|(pos, _)| pos)
            .unwrap();
        let pi = remaining.remove(pos);
        for slot in [&plans[pi].s, &plans[pi].p, &plans[pi].o] {
            if let Some(v) = slot.var() {
                bound.insert(v);
            }
        }
        order.push(pi);
    }
    order
}

fn shares_bound_var(plan: &PatPlan, bound: &std::collections::HashSet<usize>) -> bool {
    [&plan.s, &plan.p, &plan.o].iter().any(|slot| match slot {
        Slot::Const(_) => true,
        Slot::Var(v) => bound.contains(v),
    })
}

fn known_count(plan: &PatPlan, bound: &std::collections::HashSet<usize>) -> usize {
    [&plan.s, &plan.p, &plan.o]
        .iter()
        .filter(|slot| match slot {
            Slot::Const(_) => true,
            Slot::Var(v) => bound.contains(v),
        })
        .count()
}

fn var_mask(plan: &PatPlan) -> u64 {
    let mut m = 0u64;
    for slot in [&plan.s, &plan.p, &plan.o] {
        if let Some(v) = slot.var() {
            if v < 64 {
                m |= 1u64 << v;
            }
        }
    }
    m
}

fn max_var_index(plans: &[PatPlan]) -> Option<usize> {
    plans
        .iter()
        .flat_map(|p| [p.s.var(), p.p.var(), p.o.var()])
        .flatten()
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dict::Dictionary;
    use crate::model::{IdTriple, Term};

    /// Build a store where one predicate is very selective and another is broad.
    fn store_with_skew() -> (Dictionary, TripleStore) {
        let mut d = Dictionary::new();
        let rare = d.intern_predicate(&Term::iri("rare").dict_key()); // 1 triple
        let common = d.intern_predicate(&Term::iri("common").dict_key()); // many
        let mut s = TripleStore::new();
        let mut triples = vec![IdTriple::new(0, rare, 100)];
        for i in 0..50u32 {
            triples.push(IdTriple::new(i, common, 1000 + i));
        }
        s.bulk_load(triples);
        (d, s)
    }

    fn plan(s: Slot, p: Slot, o: Slot) -> PatPlan {
        PatPlan { s, p, o }
    }

    #[test]
    fn stats_are_exposed() {
        let (_d, s) = store_with_skew();
        assert_eq!(s.num_predicates(), 2);
        // 'common' predicate id is 1, 'rare' is 0
        assert_eq!(s.pred_card(0), 1);
        assert_eq!(s.pred_card(1), 50);
        assert_eq!(s.pred_distinct_subj(1), 50);
        assert_eq!(s.pred_distinct_obj(1), 50);
    }

    #[test]
    fn dp_puts_most_selective_pattern_first() {
        let (_d, s) = store_with_skew();
        // ?x rare ?y  (card 1)   and   ?x common ?z (card 50), shared var ?x (=0)
        let rare = plan(Slot::Var(0), Slot::Const(0), Slot::Var(1));
        let common = plan(Slot::Var(0), Slot::Const(1), Slot::Var(2));
        let order = order_bgp(&[common, rare], &s, &Candidates::new()); // pass broad first on purpose
                                                                        // Optimizer should evaluate the rare pattern (index 1) first.
        assert_eq!(order[0], 1, "selective pattern must lead");
    }

    #[test]
    fn single_pattern_order_is_trivial() {
        let (_d, s) = store_with_skew();
        let order = order_bgp(
            &[plan(Slot::Var(0), Slot::Const(1), Slot::Var(1))],
            &s,
            &Candidates::new(),
        );
        assert_eq!(order, vec![0]);
    }

    #[test]
    fn order_is_a_permutation_of_all_patterns() {
        let (_d, s) = store_with_skew();
        let plans = vec![
            plan(Slot::Var(0), Slot::Const(1), Slot::Var(1)),
            plan(Slot::Var(1), Slot::Const(1), Slot::Var(2)),
            plan(Slot::Var(0), Slot::Const(0), Slot::Var(3)),
        ];
        let mut order = order_bgp(&plans, &s, &Candidates::new());
        order.sort_unstable();
        assert_eq!(order, vec![0, 1, 2]);
    }
}
