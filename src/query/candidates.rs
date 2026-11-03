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

use std::collections::{HashMap, HashSet};

use crate::kvstore::StringIndex;
use crate::model::id::PredId;
use crate::store::TripleSource;

use super::engine::{FxBuildHasher, PatPlan, Slot};

/// Candidate id sets per variable index (sorted, de-duplicated). Keyed with
/// the engine's fast deterministic hasher: the executor looks a variable's set
/// up for every triple it tests.
pub(crate) type Candidates = HashMap<usize, Vec<u32>, FxBuildHasher>;

/// The cost of one membership test (a store lookup) in list entries read,
/// for the choice between testing a candidate set's members and reading a
/// whole list (here and in the engine's VS-tree refinement): a B+ tree descent
/// or a map probe against stepping through (or copying) list entries.
pub(crate) const LOOKUP_COST: u64 = 64;

/// Lists estimated at most this long are counted exactly before a
/// variable's edges are ordered; longer ones are ordered by the estimate
/// ([`ConstEdge::estimate`]): counting a long key range of an on-disk store
/// reads one B+ tree leaf per few hundred entries.
const COUNT_LIMIT: u64 = 8192;

/// One constant-neighbour edge of a variable (pass 1): the exact store list
/// of the values it allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConstEdge {
    /// `?v p o`: the subjects of `(p, o)`.
    SubjOf(PredId, u32),
    /// `?v ?x o`: the subjects of `o`, any predicate.
    SubjOfAny(u32),
    /// `s p ?v`: the objects of `(s, p)`.
    ObjOf(u32, PredId),
    /// `s ?x ?v`: the objects of `s`, any predicate.
    ObjOfAny(u32),
}

impl ConstEdge {
    /// The length of the store's list (C1 counts; for the "any predicate"
    /// edges before removing duplicates).
    fn len(self, store: &impl TripleSource) -> u64 {
        match self {
            ConstEdge::SubjOf(p, o) => store.count_s_by_po(p, o),
            ConstEdge::SubjOfAny(o) => store.count_ps_by_o(o),
            ConstEdge::ObjOf(s, p) => store.count_o_by_sp(s, p),
            ConstEdge::ObjOfAny(s) => store.count_po_by_s(s),
        }
    }

    /// The average length of such lists, from the store's O(1) statistics
    /// (only the work done depends on it, never a result).
    fn estimate(self, store: &impl TripleSource) -> u64 {
        let avg = |n: u64, keys: usize| n / (keys as u64).max(1);
        match self {
            ConstEdge::SubjOf(p, _) => avg(store.pred_card(p) as u64, store.pred_distinct_obj(p)),
            ConstEdge::ObjOf(_, p) => avg(store.pred_card(p) as u64, store.pred_distinct_subj(p)),
            ConstEdge::SubjOfAny(_) => avg(store.triple_count(), store.distinct_objects()),
            ConstEdge::ObjOfAny(_) => avg(store.triple_count(), store.distinct_subjects()),
        }
    }

    /// The list, sorted and distinct; `note` hears the exact count of a
    /// `(p, o)` / `(s, p)` list read whole.
    fn list(self, store: &impl TripleSource, note: &mut dyn FnMut(ConstEdge, u64)) -> Vec<u32> {
        let mut list = match self {
            ConstEdge::SubjOf(p, o) => store.s_by_po(p, o),
            ConstEdge::SubjOfAny(o) => distinct_seconds(&store.ps_by_o(o)),
            ConstEdge::ObjOf(s, p) => store.o_by_sp(s, p),
            ConstEdge::ObjOfAny(s) => distinct_seconds(&store.po_by_s(s)),
        };
        if matches!(self, ConstEdge::SubjOf(..) | ConstEdge::ObjOf(..)) {
            // These lists are exactly the counted ranges (C1).
            note(self, list.len() as u64);
        }
        // The contract orders these lists ascending; check rather than sort.
        if !list.windows(2).all(|w| w[0] < w[1]) {
            list.sort_unstable();
            list.dedup();
        }
        list
    }

    /// Whether `x` is in the list.
    fn holds(self, store: &impl TripleSource, x: u32) -> bool {
        match self {
            ConstEdge::SubjOf(p, o) => store.exists(x, p, o),
            ConstEdge::SubjOfAny(o) => store.count_p_by_so(x, o) > 0,
            ConstEdge::ObjOf(s, p) => store.exists(s, p, x),
            ConstEdge::ObjOfAny(s) => store.count_p_by_so(s, x) > 0,
        }
    }
}

/// Whether to test the `members` of a set against an edge whose list has
/// `len` entries rather than read the list (both give the same set).
fn test_members(members: usize, len: u64) -> bool {
    #[cfg(test)]
    if let Some(forced) = FORCE_MEMBER_TESTS.with(std::cell::Cell::get) {
        return forced;
    }
    (members as u64).saturating_mul(LOOKUP_COST) < len
}

#[cfg(test)]
thread_local! {
    /// Tests force either way of [`test_members`] on this thread.
    static FORCE_MEMBER_TESTS: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Generate candidate id sets for the BGP's variables (the engine uses
/// [`generate_noting`]).
#[cfg(test)]
pub(crate) fn generate(plans: &[PatPlan], store: &impl TripleSource) -> Candidates {
    generate_noting(plans, store, &mut |_, _| {})
}

/// [`generate`], telling `note` the exact length of every `(p, o)` / `(s, p)`
/// list it reads whole (its C1 count, for a caller that memoizes counts).
pub(crate) fn generate_noting(
    plans: &[PatPlan],
    store: &impl TripleSource,
    note: &mut dyn FnMut(ConstEdge, u64),
) -> Candidates {
    let mut cand = Candidates::default();

    // Pass 1 — constant-neighbour edges give an exact list per variable: the
    // intersection of the lists of its edges. Shortest list first (exact
    // counts for lists estimated short, the estimate for the others); a list
    // much longer than the set so far is not read — the set's members are
    // tested against the store instead (the same set: `x` is in a list iff
    // its edge exists). The result does not depend on the order.
    let mut edges: Vec<(usize, ConstEdge)> = Vec::new();
    for plan in plans {
        // subject variable with a constant object: `?s (p|?) o`
        if let (Slot::Var(sv), Slot::Const(o)) = (&plan.s, &plan.o) {
            let edge = match plan.p {
                Slot::Const(p) => ConstEdge::SubjOf(p, *o),
                Slot::Var(_) => ConstEdge::SubjOfAny(*o),
            };
            edges.push((*sv, edge));
        }
        // object variable with a constant subject: `s (p|?) ?o`
        if let (Slot::Const(s), Slot::Var(ov)) = (&plan.s, &plan.o) {
            let edge = match plan.p {
                Slot::Const(p) => ConstEdge::ObjOf(*s, p),
                Slot::Var(_) => ConstEdge::ObjOfAny(*s),
            };
            edges.push((*ov, edge));
        }
    }
    let mut vars: Vec<usize> = edges.iter().map(|&(v, _)| v).collect();
    vars.sort_unstable();
    vars.dedup();
    for var in vars {
        // (length or estimate, pattern order, edge): ties keep the pattern
        // order.
        let mut mine: Vec<(u64, usize, ConstEdge)> = edges
            .iter()
            .filter(|&&(v, _)| v == var)
            .enumerate()
            .map(|(i, &(_, e))| {
                let estimate = e.estimate(store);
                let len = if estimate <= COUNT_LIMIT {
                    e.len(store)
                } else {
                    estimate
                };
                (len, i, e)
            })
            .collect();
        mine.sort_unstable_by_key(|&(len, i, _)| (len, i));
        let mut set = mine[0].2.list(store, note);
        for &(len, _, edge) in &mine[1..] {
            if set.is_empty() {
                break;
            }
            if test_members(set.len(), len) {
                set.retain(|&x| edge.holds(store, x));
            } else {
                set = sorted_intersect(&set, &edge.list(store, note));
            }
        }
        cand.insert(var, set);
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

/// Tighten (or seed) candidate sets using mandatory-substring FILTER constraints
/// via the dictionary [`StringIndex`].
///
/// `constraints` maps a variable index to the substrings its string value MUST
/// contain (extracted from `CONTAINS`/`STRSTARTS`/`STRENDS`/plain-`REGEX`
/// filters). For a variable bound only to entity/literal ids — one used only in
/// subject/object positions — the ids whose dictionary string contains such a
/// substring are a **sound superset** of its valid bindings (a lexical substring
/// is also a substring of the serialized term), so intersecting them in (or
/// seeding, when the variable had no candidates) only prunes. Predicate-position
/// variables are skipped: they bind a separate id space the index doesn't cover.
/// The exact FILTER still runs downstream, so results never change — only work.
pub(crate) fn refine_with_substrings(
    cand: &mut Candidates,
    constraints: &HashMap<usize, Vec<String>>,
    idx: &StringIndex,
    plans: &[PatPlan],
) {
    if constraints.is_empty() {
        return;
    }
    // A variable is safe to constrain only if it binds entity/literal ids: it
    // appears in some subject/object slot and never in a predicate slot.
    let mut so_vars: HashSet<usize> = HashSet::new();
    let mut pred_vars: HashSet<usize> = HashSet::new();
    for p in plans {
        if let Slot::Var(v) = &p.s {
            so_vars.insert(*v);
        }
        if let Slot::Var(v) = &p.o {
            so_vars.insert(*v);
        }
        if let Slot::Var(v) = &p.p {
            pred_vars.insert(*v);
        }
    }
    for (&v, needles) in constraints {
        if !so_vars.contains(&v) || pred_vars.contains(&v) {
            continue;
        }
        // Intersect the per-needle candidate supersets (every needle is mandatory).
        let mut set: Option<Vec<u32>> = None;
        for n in needles {
            let c = idx.contains_candidates(n);
            set = Some(match set {
                None => c,
                Some(prev) => sorted_intersect(&prev, &c),
            });
            if set.as_deref().is_some_and(<[u32]>::is_empty) {
                break;
            }
        }
        if let Some(s) = set {
            intersect_into(cand, v, s);
        }
    }
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
    if !list.windows(2).all(|w| w[0] < w[1]) {
        list.sort_unstable();
        list.dedup();
    }
    match cand.get_mut(&var) {
        None => {
            cand.insert(var, list);
        }
        Some(existing) => {
            *existing = sorted_intersect(existing, &list);
        }
    }
}

/// Intersection of two sorted, de-duplicated slices: a binary search of each
/// member of the much shorter one in the other, else one merge pass.
fn sorted_intersect(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (small, large) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    let log = (usize::BITS - large.len().leading_zeros()) as usize;
    if small.len().saturating_mul(log) < small.len() + large.len() {
        return small
            .iter()
            .copied()
            .filter(|x| large.binary_search(x).is_ok())
            .collect();
    }
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
    fn substring_index_seeds_and_tightens_candidates() {
        use crate::kvstore::StringIndex;
        // Index over dictionary strings; ids 10 and 12 contain "MARK".
        let idx = StringIndex::build([
            (10u32, "\"alpha MARKER\""),
            (11u32, "\"beta\""),
            (12u32, "\"MARK gamma\""),
            (13u32, "\"delta\""),
        ]);
        // BGP `?s ?p ?o`: ?s(0),?o(2) in subject/object slots, ?p(1) predicate.
        let plans = vec![pp(Slot::Var(0), Slot::Var(1), Slot::Var(2))];

        // Seeds ?o (had no candidates) with the "MARK" superset {10,12}.
        let cons = HashMap::from([(2usize, vec!["MARK".to_string()])]);
        let mut cand = Candidates::default();
        refine_with_substrings(&mut cand, &cons, &idx, &plans);
        assert_eq!(cand.get(&2), Some(&vec![10, 12]));

        // A predicate-position variable is skipped (separate id space).
        let cons_p = HashMap::from([(1usize, vec!["MARK".to_string()])]);
        let mut cand_p = Candidates::default();
        refine_with_substrings(&mut cand_p, &cons_p, &idx, &plans);
        assert!(!cand_p.contains_key(&1));

        // Tightens an existing candidate set: {10,11} ∩ {10,12} = {10}.
        let mut cand_t: Candidates = [(2usize, vec![10, 11])].into_iter().collect();
        refine_with_substrings(&mut cand_t, &cons, &idx, &plans);
        assert_eq!(cand_t.get(&2), Some(&vec![10]));
    }

    /// The pass-1 algorithm before the shortest-first ordering and member
    /// tests: every list read, sorted and intersected in pattern order; pass
    /// 2 as in `generate`.
    fn reference(plans: &[PatPlan], store: &impl TripleSource) -> Candidates {
        let mut cand = Candidates::default();
        for plan in plans {
            if let (Slot::Var(sv), Slot::Const(o)) = (&plan.s, &plan.o) {
                let list = match plan.p {
                    Slot::Const(p) => store.s_by_po(p, *o),
                    Slot::Var(_) => distinct_seconds(&store.ps_by_o(*o)),
                };
                intersect_into(&mut cand, *sv, list);
            }
            if let (Slot::Const(s), Slot::Var(ov)) = (&plan.s, &plan.o) {
                let list = match plan.p {
                    Slot::Const(p) => store.o_by_sp(*s, p),
                    Slot::Var(_) => distinct_seconds(&store.po_by_s(*s)),
                };
                intersect_into(&mut cand, *ov, list);
            }
        }
        for plan in plans {
            if let Slot::Const(p) = plan.p {
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

    #[test]
    fn generate_equals_reading_every_list() {
        // xorshift64*
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move |n: u32| {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            (state.wrapping_mul(0x2545_f491_4f6c_dd1d) % u64::from(n)) as u32
        };
        for round in 0..20 {
            // Skewed: a few objects and predicates carry most triples.
            let mut triples = Vec::new();
            for _ in 0..600 {
                let o = if next(3) == 0 { next(60) } else { next(4) };
                let bound = 1 + next(8);
                triples.push(IdTriple::new(next(60), next(bound), o));
            }
            let mut s = TripleStore::new();
            s.bulk_load(triples);
            for _ in 0..60 {
                let slot = |next: &mut dyn FnMut(u32) -> u32, consts: u32| {
                    if next(2) == 0 {
                        Slot::Var(next(3) as usize)
                    } else {
                        Slot::Const(next(consts))
                    }
                };
                let plans: Vec<PatPlan> = (0..1 + next(4))
                    .map(|_| {
                        let s_ = slot(&mut next, 60);
                        let p = if next(5) == 0 {
                            Slot::Var(10 + next(2) as usize)
                        } else {
                            Slot::Const(next(8))
                        };
                        let o = slot(&mut next, 60);
                        pp(s_, p, o)
                    })
                    .collect();
                let want = reference(&plans, &s);
                for forced in [None, Some(true), Some(false)] {
                    FORCE_MEMBER_TESTS.with(|f| f.set(forced));
                    let got = generate(&plans, &s);
                    FORCE_MEMBER_TESTS.with(|f| f.set(None));
                    assert_eq!(got, want, "round {round}, member tests {forced:?}");
                }
            }
        }
    }

    #[test]
    fn sorted_intersect_takes_either_route() {
        let a: Vec<u32> = (0..1000).filter(|x| x % 3 == 0).collect();
        let b: Vec<u32> = (0..1000).filter(|x| x % 5 == 0).collect();
        let want: Vec<u32> = (0..1000).filter(|x| x % 15 == 0).collect();
        assert_eq!(sorted_intersect(&a, &b), want);
        let small = vec![0, 15, 16, 990, 999];
        assert_eq!(sorted_intersect(&small, &a), vec![0, 15, 990, 999]);
        assert_eq!(sorted_intersect(&a, &small), vec![0, 15, 990, 999]);
        assert!(sorted_intersect(&[], &a).is_empty());
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
