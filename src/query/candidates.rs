//! Exact per-variable candidate generation.
//!
//! Port of gStore's constant-edge candidate filtering (`FilterPlan::
//! OnlyConstFilter` + `Executor::CacheConstantCandidates`, then
//! `PlanGenerator::CompleteCandidate`). For each so-variable we intersect the
//! index lists implied by its *constant-neighbour* edges, giving an exact
//! candidate id set — far tighter than (and superseding) the signature filter.
//! A second propagation pass intersects with selective predicate populations.
//!
//! Every list used is a superset-free exact set for that single edge, and
//! intersection only shrinks, so the result is exactly the set of ids that
//! satisfy all the variable's constant-edge constraints — sound and precise.

use std::collections::HashMap;

use crate::store::TripleSource;

use super::engine::{PatPlan, Slot};

/// Candidate id sets per variable index (sorted, de-duplicated).
pub(crate) type Candidates = HashMap<usize, Vec<u32>>;

/// Generate candidate id sets for the BGP's variables.
pub(crate) fn generate(plans: &[PatPlan], store: &impl TripleSource) -> Candidates {
    let mut cand: Candidates = HashMap::new();

    // Pass 1 — constant-neighbour edges give an exact list per variable.
    for plan in plans {
        // subject variable with a constant object: `?s (p|?) o`
        if let (Slot::Var(sv), Slot::Const(o)) = (&plan.s, &plan.o) {
            let list = match plan.p {
                Slot::Const(p) => store.s_by_po(p, *o),
                Slot::Var(_) => distinct_seconds(&store.ps_by_o(*o)), // subjects of o (any pred)
            };
            intersect_into(&mut cand, *sv, list);
        }
        // object variable with a constant subject: `s (p|?) ?o`
        if let (Slot::Const(s), Slot::Var(ov)) = (&plan.s, &plan.o) {
            let list = match plan.p {
                Slot::Const(p) => store.o_by_sp(*s, p),
                Slot::Var(_) => distinct_seconds(&store.po_by_s(*s)), // objects of s (any pred)
            };
            intersect_into(&mut cand, *ov, list);
        }
    }

    // Pass 2 — propagation: for a variable that already has candidates, a
    // selective constant predicate on a *variable*-neighbour edge further
    // constrains it to that predicate's subject/object population.
    for plan in plans {
        if let Slot::Const(p) = plan.p {
            // `?s p ?o`: ?s must be a subject of p, ?o an object of p.
            if let (Slot::Var(sv), Slot::Var(_)) = (&plan.s, &plan.o) {
                if should_propagate(&cand, *sv, store.pred_card(p)) {
                    intersect_into(&mut cand, *sv, store.subs_by_p(p));
                }
            }
            if let (Slot::Var(_), Slot::Var(ov)) = (&plan.s, &plan.o) {
                if should_propagate(&cand, *ov, store.pred_card(p)) {
                    intersect_into(&mut cand, *ov, store.objs_by_p(p));
                }
            }
        }
    }

    cand
}

/// Only propagate when the variable already has candidates and the predicate is
/// selective relative to them (gStore's `size / (log2(size)+1)` border) — keeps
/// the (always-sound) intersection cheap.
fn should_propagate(cand: &Candidates, var: usize, pred_card: usize) -> bool {
    match cand.get(&var) {
        None => false, // no base candidates ⇒ skip (would scan the whole predicate)
        Some(c) => {
            let size = c.len().max(2) as f64;
            let border = size / (size.log2() + 1.0);
            (pred_card as f64) <= border.max(1.0)
        }
    }
}

/// Distinct second components of `(a, b)` pairs, sorted.
fn distinct_seconds(pairs: &[(u32, u32)]) -> Vec<u32> {
    let mut v: Vec<u32> = pairs.iter().map(|&(_, b)| b).collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// Intersect `list` into the candidate set of `var` (creating it if absent).
fn intersect_into(cand: &mut Candidates, var: usize, mut list: Vec<u32>) {
    list.sort_unstable();
    list.dedup();
    match cand.get_mut(&var) {
        None => {
            cand.insert(var, list);
        }
        Some(existing) => {
            *existing = sorted_intersect(existing, &list);
        }
    }
}

/// Intersection of two sorted, de-duplicated slices.
fn sorted_intersect(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::IdTriple;
    use crate::store::TripleStore;

    fn store() -> TripleStore {
        // preds: type=0, takesCourse=1, name=2
        // grad students 1,2,3 are type=GradStudent(100); only 2 takes course0(200)
        let mut s = TripleStore::new();
        s.bulk_load(vec![
            IdTriple::new(1, 0, 100),
            IdTriple::new(2, 0, 100),
            IdTriple::new(3, 0, 100),
            IdTriple::new(2, 1, 200),
            IdTriple::new(5, 1, 200), // 5 takes course0 but isn't a grad student
            IdTriple::new(2, 2, 2_000_000_000),
        ]);
        s
    }

    fn pp(s: Slot, p: Slot, o: Slot) -> PatPlan {
        PatPlan { s, p, o }
    }

    #[test]
    fn intersects_constant_edges_to_exact_set() {
        let s = store();
        // ?x type GradStudent(100) . ?x takesCourse course0(200)   (?x = var 0)
        let plans = vec![
            pp(Slot::Var(0), Slot::Const(0), Slot::Const(100)),
            pp(Slot::Var(0), Slot::Const(1), Slot::Const(200)),
        ];
        let c = generate(&plans, &s);
        // s_by_po(type,Grad) = {1,2,3}; s_by_po(takes,c0) = {2,5}; ∩ = {2}
        assert_eq!(c.get(&0), Some(&vec![2]));
    }

    #[test]
    fn single_constant_edge_gives_that_list() {
        let s = store();
        let plans = vec![pp(Slot::Var(0), Slot::Const(0), Slot::Const(100))];
        let c = generate(&plans, &s);
        assert_eq!(c.get(&0), Some(&vec![1, 2, 3]));
    }

    #[test]
    fn object_variable_from_constant_subject() {
        let s = store();
        // <2> takesCourse ?o   → ?o = {200}
        let plans = vec![pp(Slot::Const(2), Slot::Const(1), Slot::Var(0))];
        let c = generate(&plans, &s);
        assert_eq!(c.get(&0), Some(&vec![200]));
    }

    #[test]
    fn no_constant_edge_means_no_candidate() {
        let s = store();
        // ?x ?p ?y — nothing constant
        let plans = vec![pp(Slot::Var(0), Slot::Var(1), Slot::Var(2))];
        let c = generate(&plans, &s);
        assert!(c.is_empty());
    }
}
