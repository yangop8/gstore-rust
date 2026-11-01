//! Top-K, bounded-heap `ORDER BY` and early termination for `LIMIT`, `ASK`
//! and `EXISTS` (docs/GAP_PLAN.md §3.4).
//!
//! The evaluator in [`engine`](super) materializes every solution of a WHERE
//! clause, then sorts, de-duplicates and cuts. This module adds three ways to
//! do less work, each returning **exactly** the rows the full evaluation
//! returns — same rows, same order, including the order among ties — so
//! switching them off ([`TopKOptions`]) never changes a result
//! (`tests/dt_topk.rs` checks that over randomized data and queries).
//!
//! # Early termination (streaming)
//!
//! A SELECT with `LIMIT` and no `ORDER BY` (and no GROUP BY, aggregate,
//! HAVING or projected graph function), a CONSTRUCT with `LIMIT` and no
//! `ORDER BY`, `ASK`, and `EXISTS` / `NOT EXISTS` pull their WHERE solutions
//! one at a time and stop after OFFSET+LIMIT projected rows (distinct rows
//! for `SELECT DISTINCT`, counted in a seen-set) or after the first
//! solution. Solutions arrive in the order the full evaluation produces them,
//! so the rows are the same. How far each pattern streams:
//!
//! * **BGP** — fully: the left-deep plan runs depth first (one candidate
//!   list per partial solution, no intermediate result), a bushy plan streams
//!   its left operand and probes the materialized right one. Both use the
//!   plan the full evaluation would use (same candidates, same plan cache).
//!   A BGP with a *mixed* variable (a predicate variable also bound as a
//!   subject/object, see `VarLayout::pred_alias`) runs its left-deep order
//!   depth first with the same id translation as `eval_mixed_pipeline`.
//! * **FILTER, BIND, UNION, `GRAPH`** — fully (`UNION` streams its left
//!   branch, then its right one; `GRAPH ?g` each named graph in turn).
//! * **OPTIONAL, MINUS** — the left side streams; the right side is
//!   evaluated in full once, on the first left solution.
//! * **A group of several conjuncts** (`{ … } { … }`, a BGP next to
//!   `VALUES`, a sub-SELECT, a path, …) — each conjunct is evaluated in full,
//!   because their sizes decide the join order; the join itself then streams
//!   (depth first over hash-indexed conjuncts, no intermediate result).
//! * **VALUES, sub-SELECT, property path, SERVICE** — evaluated in full
//!   (a sub-SELECT applies these optimizations to itself).
//!
//! # Top-K with a bounded heap
//!
//! A SELECT (or CONSTRUCT) with `ORDER BY` and `LIMIT` streams its solutions
//! through a max-heap of the OFFSET+LIMIT best ones — `O(n log k)` time,
//! `O(k)` memory instead of a full sort. The keys are the full path's keys
//! (SPARQL §15.1 order: unbound < blank nodes < IRIs < literals, ASC/DESC,
//! several keys, SELECT aliases) and ties are broken by the position of the
//! solution in the WHERE solution sequence, which is what the full path's
//! stable sort does. With `DISTINCT`, the heap holds the best distinct
//! projected rows, each ranked by its best occurrence.
//!
//! # DP-B
//!
//! For `ORDER BY [ASC|DESC](linear expression)` + `LIMIT k` over a
//! tree-shaped BGP, a port of upstream's DP-B Top-K
//! (`src/Query/topk/`: `DPBTopKUtil`, `TopKSearchPlan`, `DPB/OrderedList`)
//! enumerates the complete matches in key order instead of computing all of
//! them. As upstream, the BGP's variables form a tree; each variable value
//! `v` gets an `FQ` list (the matches of its subtree in increasing score,
//! combining its children's lists lazily) and each parent value / child
//! variable pair an `FR` list (merging the `FQ` lists of the children).
//! Upstream's `OW` lists (leaf children) are `FR` lists of single-element
//! `FQ`s here. Only as many list elements as needed are computed.
//!
//! Supported shapes (anything else falls back to the bounded heap):
//!
//! * the WHERE clause is one BGP, optionally under `FILTER`s (filtered
//!   matches do not count towards `k`), without a trailing `VALUES`;
//! * the subject/object variables and the triple patterns joining two of
//!   them form a tree (several patterns between the same two variables are
//!   one tree edge); a pattern with one variable (and constants), or the
//!   same variable twice, restricts that variable; a predicate variable
//!   must occur in exactly one pattern, between two different variables,
//!   and not also as a subject or object (a mixed variable);
//! * the first `ORDER BY` key is `Σ cᵢ·?vᵢ + c₀` built from variables,
//!   integer constants, `+`, `-`, unary `-`/`+` and `*` by a constant (the
//!   later keys, if any, may be anything);
//! * every value any match gives those variables is an integer (`xsd:integer`
//!   and its derived types within `i64`), and the coefficients stay small
//!   (`Σ|cᵢ| ≤ 2³²`). Then the scores are exact, so the match order is the
//!   key order SPARQL defines. Upstream instead parses any lexical form as a
//!   `double` and drops matches whose values are not numbers, which SPARQL
//!   sorts first (ASC) or last (DESC); here such data takes the heap path.
//!
//! DP-B reads every candidate value of the tree once (the semi-join
//! reduction), at about the cost the heap path pays per match, so it pays
//! off when there are more matches than values — a star or a one-to-many
//! edge — not on a chain of one-to-one edges, where the heap is cheaper
//! (`examples/topk_bench.rs`: 3× slower there, 12× faster on a star with
//! fan-out 5, 100× with fan-out 20). Upstream runs DP-B on every such query;
//! here the default [`DpbMode::Auto`] estimates the matches (root values
//! times each tree edge's average fan-out, from the store's per-predicate
//! statistics) and runs DP-B only when they outnumber the values it reads
//! at least twice; [`DpbMode::Always`] skips the estimate.
//!
//! Upstream's entry point (`Optimizer::ChooseStrategy` / `ExecutionTopK`)
//! takes DP-B for a LIMIT with exactly one `ORDER BY` expression that is not
//! a bare variable; here a bare variable is a linear key too, and later keys
//! are allowed. Two upstream behaviours are deliberately not copied: ties
//! come out in DP-B's internal order there (here in the full path's order),
//! and `FQIterator::TryGetNext` leaves a scored node's own score out of its
//! list's first element but adds it to every later one as it enters the
//! pool, while the frontier mixes costs with and without it — misranking
//! matches when a scored variable has children in the tree; here every
//! element's score includes it exactly once.
//!
//! Cycles, which upstream cuts and re-checks, also take the heap path. DP-B
//! enumerates until it has `k` matches and all matches tied with the `k`-th
//! — a prefix of the sorted solution sequence — and orders those by the full
//! keys and by their position in the BGP's own solution order, computed from
//! the executor's plan; past `8k + 10000` matches (many ties, or a selective
//! FILTER) it gives up and the heap path runs instead.
//!
//! # Cancellation and memory
//!
//! The streaming executor polls the cancel flag at every BGP step and every
//! filtered solution, DP-B before every match; a cancelled query still
//! reports [`GStoreError::Cancelled`](crate::error::GStoreError::Cancelled).
//! Streaming keeps one candidate list per BGP depth; the heap `O(k)`
//! solutions; DP-B the semi-join-reduced edges of the tree plus the list
//! elements it computed.

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, HashSet};
use std::ops::ControlFlow;

use super::*;

/// Which of the `LIMIT` / `ORDER BY` / `ASK` optimizations an
/// [`Evaluator`] may use ([`Evaluator::with_topk_options`]). All are on by
/// default. Each returns exactly the rows the full evaluation returns, so
/// switching one off only changes how much work a query does — the switch
/// exists for testing and benchmarking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopKOptions {
    /// Stream the WHERE solutions and stop once they are enough: SELECT /
    /// CONSTRUCT with `LIMIT` and no `ORDER BY`, `ASK`, `EXISTS`.
    pub early_termination: bool,
    /// `ORDER BY` + `LIMIT`: keep the best OFFSET+LIMIT solutions in a
    /// bounded heap instead of sorting all of them.
    pub bounded_heap: bool,
    /// `ORDER BY` linear expression + `LIMIT` over a tree-shaped BGP:
    /// enumerate the matches in key order (DP-B).
    pub dpb: DpbMode,
}

/// When DP-B answers an `ORDER BY … LIMIT` query whose shape it handles
/// ([`TopKOptions::dpb`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DpbMode {
    /// Never: the bounded heap (or the full sort) answers.
    Off,
    /// When the store's statistics predict clearly more matches than DP-B
    /// reads (the default): DP-B touches every candidate value of the tree
    /// once, so on a query with about as many matches as values (a chain of
    /// one-to-one edges) the heap is cheaper.
    Auto,
    /// Whenever the shape allows (for tests and benchmarks).
    Always,
}

impl TopKOptions {
    /// Every optimization on (the default).
    pub const ALL: TopKOptions = TopKOptions {
        early_termination: true,
        bounded_heap: true,
        dpb: DpbMode::Auto,
    };
    /// Every optimization off: always evaluate in full, then sort and cut.
    pub const NONE: TopKOptions = TopKOptions {
        early_termination: false,
        bounded_heap: false,
        dpb: DpbMode::Off,
    };
}

impl Default for TopKOptions {
    fn default() -> TopKOptions {
        TopKOptions::ALL
    }
}

/// Work counters of an [`Evaluator`] ([`Evaluator::stats`]), summed over
/// every query it evaluated (and the `GRAPH` blocks inside them).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EvalStats {
    /// BGP solutions produced, partial or complete: one per successful
    /// extension of a solution by a triple pattern, and one per solution a
    /// bushy plan's join produces.
    pub bgp_bindings: u64,
    /// Evaluations (SELECT, sub-SELECT, CONSTRUCT, ASK, EXISTS) that streamed
    /// their WHERE solutions, able to stop early.
    pub streamed: u64,
    /// `ORDER BY` + `LIMIT` evaluations that kept the best solutions in a
    /// bounded heap.
    pub heap_topk: u64,
    /// `ORDER BY` + `LIMIT` evaluations answered by DP-B.
    pub dpb_topk: u64,
    /// Complete BGP matches DP-B enumerated (in score order).
    pub dpb_matches: u64,
}

/// What a streaming consumer tells the producer: go on, or stop.
pub(super) type Flow = ControlFlow<()>;
const GO: Flow = ControlFlow::Continue(());
const STOP: Flow = ControlFlow::Break(());

/// A consumer of streamed solutions.
type Sink<'s> = dyn FnMut(Binding) -> Flow + 's;

/// A BGP ready to run ([`Evaluator::prepare_bgp`]).
pub(super) enum PreparedBgp {
    /// A constant is not in the dictionary: no solution.
    Empty,
    /// No triple pattern: the one empty solution.
    Identity,
    /// Compiled patterns, their candidate sets and the execution plan.
    Plan(BgpPlan),
}

/// The compiled form of a non-empty BGP (see [`PreparedBgp`]).
pub(super) struct BgpPlan {
    pub(super) plans: Vec<PatPlan>,
    pub(super) cands: Candidates,
    pub(super) exec: ExecPlan,
}

impl<'a, S: TripleSource> Evaluator<'a, S> {
    /// Choose which `LIMIT` / `ORDER BY` / `ASK` optimizations may run (see
    /// [`TopKOptions`]; all are on by default). Builder-style.
    pub fn with_topk_options(mut self, options: TopKOptions) -> Evaluator<'a, S> {
        self.topk = options;
        self
    }

    /// The optimizations this evaluator may use.
    pub fn topk_options(&self) -> TopKOptions {
        self.topk
    }

    /// The work counters so far (see [`EvalStats`]).
    pub fn stats(&self) -> EvalStats {
        self.stats.get()
    }

    /// Reset the work counters to zero.
    pub fn reset_stats(&self) {
        self.stats.set(EvalStats::default());
    }

    fn bump(&self, f: impl FnOnce(&mut EvalStats)) {
        let mut s = self.stats.get();
        f(&mut s);
        self.stats.set(s);
    }

    /// Count `n` BGP solutions produced by a triple-pattern extension.
    pub(super) fn note_bindings(&self, n: usize) {
        if n > 0 {
            self.bump(|s| s.bgp_bindings += n as u64);
        }
    }

    // ---- early termination ---------------------------------------------

    /// Whether `gp` has a solution — for `ASK` and `EXISTS`: stops at the
    /// first one when early termination is on.
    pub(super) fn pattern_has_solution(&self, gp: &GraphPattern, layout: &VarLayout) -> bool {
        if !self.topk.early_termination {
            return !self.eval_pattern(gp, layout).is_empty();
        }
        self.bump(|s| s.streamed += 1);
        let mut found = false;
        let _ = self.stream_pattern(gp, layout, &mut |_| {
            found = true;
            STOP
        });
        found
    }

    /// The first `n` solutions of `gp`, in the full evaluation's order. With
    /// early termination off, all of them (the caller cuts).
    pub(super) fn first_solutions(
        &self,
        gp: &GraphPattern,
        layout: &VarLayout,
        n: usize,
    ) -> Vec<Binding> {
        if !self.topk.early_termination {
            return self.eval_pattern(gp, layout);
        }
        self.bump(|s| s.streamed += 1);
        let mut out = Vec::new();
        if n > 0 {
            let _ = self.stream_pattern(gp, layout, &mut |b| {
                out.push(b);
                if out.len() >= n {
                    STOP
                } else {
                    GO
                }
            });
        }
        out
    }

    /// Feed the solutions of `gp` to `sink` one at a time, in exactly the
    /// order [`eval_pattern`](Self::eval_pattern) returns them, until `sink`
    /// stops (or cancellation, which the caller reports). See the module docs
    /// for how far each kind of pattern streams.
    pub(super) fn stream_pattern(
        &self,
        gp: &GraphPattern,
        layout: &VarLayout,
        sink: &mut Sink<'_>,
    ) -> Flow {
        match gp {
            GraphPattern::Empty => sink(vec![None; layout.len()]),
            GraphPattern::Bgp(tps) => self.stream_bgp(tps, layout, &HashMap::new(), sink),
            GraphPattern::Union(a, b) => {
                self.stream_pattern(a, layout, sink)?;
                self.stream_pattern(b, layout, sink)
            }
            GraphPattern::Filter(filters, inner) => {
                let mut keep = |b: Binding| -> Flow {
                    if self.cancelled() {
                        return STOP;
                    }
                    if filters
                        .iter()
                        .all(|f| self.eval_ebv(f, &b, layout) == Some(true))
                    {
                        sink(b)
                    } else {
                        GO
                    }
                };
                match inner.as_ref() {
                    GraphPattern::Bgp(tps) => {
                        let cons = self.filter_string_constraints(filters, layout);
                        self.stream_bgp(tps, layout, &cons, &mut keep)
                    }
                    _ => self.stream_pattern(inner, layout, &mut keep),
                }
            }
            GraphPattern::Join(_, _) => {
                let mut conjuncts = Vec::new();
                flatten_join(gp, &mut conjuncts);
                self.stream_join(&conjuncts, layout, sink)
            }
            GraphPattern::LeftJoin(a, b, filters) => {
                let avars = pattern_vars(a, layout);
                let hint: Vec<usize> = pattern_vars(b, layout)
                    .into_iter()
                    .filter(|v| avars.contains(v))
                    .collect();
                let mut right: Option<Probe> = None;
                self.stream_pattern(a, layout, &mut |l| {
                    let probe = right
                        .get_or_insert_with(|| Probe::new(self.eval_pattern(b, layout), &hint));
                    let mut matched = false;
                    probe.for_each_compatible(&l, &mut |m| {
                        if filters
                            .iter()
                            .all(|f| self.eval_ebv(f, &m, layout) == Some(true))
                        {
                            matched = true;
                            sink(m)
                        } else {
                            GO
                        }
                    })?;
                    if matched {
                        GO
                    } else {
                        sink(l)
                    }
                })
            }
            GraphPattern::Minus(a, b) => {
                let lvars = pattern_vars(a, layout);
                let rvars = pattern_vars(b, layout);
                let shared: Vec<usize> = lvars
                    .iter()
                    .copied()
                    .filter(|v| rvars.contains(v))
                    .collect();
                if shared.is_empty() {
                    // No shared variable: MINUS removes nothing.
                    return self.stream_pattern(a, layout, sink);
                }
                let mut right: Option<Vec<Binding>> = None;
                self.stream_pattern(a, layout, &mut |l| {
                    let right = right.get_or_insert_with(|| self.eval_pattern(b, layout));
                    if minus_removes(&l, right, &shared) {
                        GO
                    } else {
                        sink(l)
                    }
                })
            }
            GraphPattern::Extend(inner, var, expr) => {
                let vi = layout.index[var];
                self.stream_pattern(inner, layout, &mut |mut b| {
                    // BIND error/unbound leaves the variable unbound (no row drop).
                    if let Some(id) = self.eval_id(expr, &b, layout) {
                        b[vi] = Some(id);
                    }
                    sink(b)
                })
            }
            GraphPattern::Graph(g, inner) => self.stream_graph(g, inner, layout, sink),
            GraphPattern::Values(..)
            | GraphPattern::SubSelect(_)
            | GraphPattern::Path(_)
            | GraphPattern::Service { .. } => {
                for b in self.eval_pattern(gp, layout) {
                    sink(b)?;
                }
                GO
            }
        }
    }

    /// Stream a `GRAPH` pattern (see [`eval_graph`](Self::eval_graph)).
    fn stream_graph(
        &self,
        g: &GraphTerm,
        inner: &GraphPattern,
        layout: &VarLayout,
        sink: &mut Sink<'_>,
    ) -> Flow {
        let Some(named) = self.named else {
            return GO;
        };
        match g {
            GraphTerm::Iri(iri) => {
                let Some(gid) = self.dict.entity_id(&Term::iri(iri.clone()).dict_key()) else {
                    return GO;
                };
                match named.get(&gid) {
                    Some(gstore) => self
                        .graph_sub_evaluator(gstore, named)
                        .stream_pattern(inner, layout, sink),
                    None => GO,
                }
            }
            GraphTerm::Var(v) => {
                let vi = layout.index[v];
                for (&gid, gstore) in named {
                    let sub = self.graph_sub_evaluator(gstore, named);
                    sub.stream_pattern(inner, layout, &mut |mut b| {
                        b[vi] = Some(gid);
                        sink(b)
                    })?;
                }
                GO
            }
        }
    }

    /// Stream a group of conjuncts (see [`eval_join`](Self::eval_join)):
    /// every conjunct is evaluated in full, as their sizes decide the join
    /// order; the join then runs depth first.
    fn stream_join(
        &self,
        conjuncts: &[&GraphPattern],
        layout: &VarLayout,
        sink: &mut Sink<'_>,
    ) -> Flow {
        let mut parts: Vec<Option<(Vec<Binding>, Vec<usize>)>> = conjuncts
            .iter()
            .map(|c| Some((self.eval_pattern(c, layout), pattern_vars(c, layout))))
            .collect();
        let order = {
            let meta: Vec<(usize, &[usize])> = parts
                .iter()
                .map(|p| {
                    p.as_ref()
                        .map(|(b, v)| (b.len(), v.as_slice()))
                        .unwrap_or_default()
                })
                .collect();
            greedy_join_order(&meta)
        };
        let Some((&first, rest)) = order.split_first() else {
            return GO;
        };
        let (start, mut acc_vars) = parts[first].take().unwrap_or_default();
        let mut probes = Vec::with_capacity(rest.len());
        for &i in rest {
            let (rows, vars) = parts[i].take().unwrap_or_default();
            let shared: Vec<usize> = acc_vars
                .iter()
                .copied()
                .filter(|v| vars.contains(v))
                .collect();
            probes.push(Probe::new(rows, &shared));
            for v in vars {
                if !acc_vars.contains(&v) {
                    acc_vars.push(v);
                }
            }
        }
        for b in start {
            join_dfs(&probes, b, sink)?;
        }
        GO
    }

    /// Stream a BGP with the plan [`eval_bgp_constrained`](Self::eval_bgp_constrained)
    /// would use.
    fn stream_bgp(
        &self,
        tps: &[TriplePattern],
        layout: &VarLayout,
        str_constraints: &HashMap<usize, Vec<String>>,
        sink: &mut Sink<'_>,
    ) -> Flow {
        match self.prepare_bgp(tps, layout, str_constraints) {
            PreparedBgp::Empty => GO,
            PreparedBgp::Identity => sink(vec![None; layout.len()]),
            PreparedBgp::Plan(p) => {
                // A mixed variable in predicate position: the left-deep order
                // with id translation, as `eval_bgp_constrained` runs it.
                let mixed = mixed_slots(&p.plans, layout);
                if !mixed.is_empty() {
                    let mut caches = MixedCaches::default();
                    let start = vec![None; layout.len()];
                    return self.stream_mixed(&p, &mixed, &mut caches, 0, start, sink);
                }
                match &p.exec.tree {
                    Some(tree) => self.stream_join_tree(tree, &p, layout, sink),
                    None => self.stream_left_deep(&p, 0, vec![None; layout.len()], sink),
                }
            }
        }
    }

    /// [`eval_mixed_pipeline`](Self::eval_mixed_pipeline), depth first:
    /// extend `binding` by the `depth`-th pattern of the left-deep order,
    /// translating the mixed variables' ids between the two dictionaries.
    fn stream_mixed(
        &self,
        p: &BgpPlan,
        mixed: &[(usize, usize)],
        caches: &mut MixedCaches,
        depth: usize,
        mut binding: Binding,
        sink: &mut Sink<'_>,
    ) -> Flow {
        let Some(&pi) = p.exec.order.get(depth) else {
            mixed_clear(&mut binding, mixed);
            return sink(binding);
        };
        if self.cancelled() {
            return STOP;
        }
        let plan = &p.plans[pi];
        if !self.mixed_enter(&mut binding, plan, mixed, caches) {
            return GO;
        }
        let mut fresh = Vec::new();
        self.extend(&binding, plan, &p.cands, &mut fresh);
        for mut nb in fresh {
            if self.mixed_leave(&mut nb, mixed, caches) {
                self.stream_mixed(p, mixed, caches, depth + 1, nb, sink)?;
            }
        }
        GO
    }

    /// The left-deep pipeline, depth first: extend `binding` by the
    /// `depth`-th pattern of the plan, one candidate at a time.
    fn stream_left_deep(
        &self,
        p: &BgpPlan,
        depth: usize,
        binding: Binding,
        sink: &mut Sink<'_>,
    ) -> Flow {
        let Some(&pi) = p.exec.order.get(depth) else {
            return sink(binding);
        };
        if self.cancelled() {
            return STOP;
        }
        let plan = &p.plans[pi];
        // As `extend`: the candidate sets are looked up once, and a small one
        // turns a predicate scan into lookups (same triples, same order).
        let s_cands = slot_candidates(&plan.s, &p.cands);
        let o_cands = slot_candidates(&plan.o, &p.cands);
        for (s, pr, o) in self.candidate_triples_in(&binding, plan, s_cands, o_cands) {
            if !in_candidates(s_cands, s) || !in_candidates(o_cands, o) {
                continue;
            }
            let mut nb = binding.clone();
            if unify(&mut nb, &plan.s, s)
                && unify(&mut nb, &plan.p, pr)
                && unify(&mut nb, &plan.o, o)
            {
                self.note_bindings(1);
                self.stream_left_deep(p, depth + 1, nb, sink)?;
            }
        }
        GO
    }

    /// A bushy plan (see [`eval_join_tree`](Self::eval_join_tree)): stream
    /// the left operand, probe the right one, materialized on first use.
    fn stream_join_tree(
        &self,
        tree: &JoinTree,
        p: &BgpPlan,
        layout: &VarLayout,
        sink: &mut Sink<'_>,
    ) -> Flow {
        match tree {
            JoinTree::Leaf(i) => {
                if self.cancelled() {
                    return STOP;
                }
                let plan = &p.plans[*i];
                let empty = vec![None; layout.len()];
                let s_cands = slot_candidates(&plan.s, &p.cands);
                let o_cands = slot_candidates(&plan.o, &p.cands);
                for (s, pr, o) in self.candidate_triples_in(&empty, plan, s_cands, o_cands) {
                    if !in_candidates(s_cands, s) || !in_candidates(o_cands, o) {
                        continue;
                    }
                    let mut nb = empty.clone();
                    if unify(&mut nb, &plan.s, s)
                        && unify(&mut nb, &plan.p, pr)
                        && unify(&mut nb, &plan.o, o)
                    {
                        self.note_bindings(1);
                        sink(nb)?;
                    }
                }
                GO
            }
            JoinTree::Join(l, r) => {
                let lv = tree_vars(l, &p.plans);
                let shared: Vec<usize> = tree_vars(r, &p.plans)
                    .into_iter()
                    .filter(|v| lv.contains(v))
                    .collect();
                let mut right: Option<Probe> = None;
                self.stream_join_tree(l, p, layout, &mut |lb| {
                    let probe = right.get_or_insert_with(|| {
                        Probe::new(
                            self.eval_join_tree(r, &p.plans, &p.cands, layout).0,
                            &shared,
                        )
                    });
                    probe.for_each_compatible(&lb, &mut |m| {
                        self.note_bindings(1);
                        sink(m)
                    })
                })
            }
        }
    }

    // ---- SELECT / CONSTRUCT with LIMIT -----------------------------------

    /// The fast paths of a SELECT without grouping, aggregates or graph
    /// functions: with `LIMIT` and no `ORDER BY`, stream and stop after
    /// OFFSET+LIMIT (distinct) rows; with `ORDER BY` and `LIMIT`, DP-B or the
    /// bounded heap. Returns a prefix of the projected rows the full path
    /// would produce (before DISTINCT/OFFSET/LIMIT), long enough for them —
    /// or `None` when no fast path applies.
    #[allow(clippy::type_complexity)]
    pub(super) fn select_fast_path(
        &self,
        q: &SelectQuery,
        pattern: &GraphPattern,
        layout: &VarLayout,
        result_vars: &[String],
    ) -> Result<Option<Vec<Vec<Option<u32>>>>> {
        let Some(limit) = q.limit else {
            return Ok(None);
        };
        let k = q.offset.unwrap_or(0).saturating_add(limit);
        if q.order_by.is_empty() {
            if !self.topk.early_termination {
                return Ok(None);
            }
            self.bump(|s| s.streamed += 1);
            let mut rows = Vec::new();
            let mut seen = HashSet::new();
            let mut failed = None;
            if k > 0 {
                let _ = self.stream_pattern(pattern, layout, &mut |b| match self.project_items(
                    &b,
                    layout,
                    q,
                    result_vars,
                ) {
                    Ok(row) => {
                        if !q.distinct || seen.insert(row.clone()) {
                            rows.push(row);
                        }
                        if rows.len() >= k {
                            STOP
                        } else {
                            GO
                        }
                    }
                    Err(e) => {
                        failed = Some(e);
                        STOP
                    }
                });
            }
            self.check_cancel()?;
            return match failed {
                Some(e) => Err(e),
                None => Ok(Some(rows)),
            };
        }
        if !self.topk.bounded_heap && self.topk.dpb == DpbMode::Off {
            return Ok(None);
        }
        let ctx = OrderCtx::new(layout, &q.order_by, Some(&q.projection));
        if k == 0 {
            return Ok(Some(Vec::new()));
        }
        if self.topk.dpb != DpbMode::Off {
            if let Some(sols) = self.dpb_top_k(q, pattern, layout, &ctx, result_vars, k)? {
                self.bump(|s| s.dpb_topk += 1);
                return sols
                    .iter()
                    .map(|b| self.project_items(b, &ctx.full, q, result_vars))
                    .collect::<Result<Vec<_>>>()
                    .map(Some);
            }
        }
        if !self.topk.bounded_heap {
            return Ok(None);
        }
        self.bump(|s| s.heap_topk += 1);
        let rows = if q.distinct {
            self.heap_top_k_distinct(q, pattern, layout, &ctx, result_vars, k)?
        } else {
            let best = self.heap_top_k(pattern, layout, &ctx, k);
            self.check_cancel()?;
            best.iter()
                .map(|b| self.project_items(b, &ctx.full, q, result_vars))
                .collect::<Result<Vec<_>>>()?
        };
        Ok(Some(rows))
    }

    /// The CONSTRUCT counterpart of [`select_fast_path`](Self::select_fast_path):
    /// the first (or, with `ORDER BY`, the best) OFFSET+LIMIT solutions, in
    /// order, or `None` when no fast path applies.
    pub(super) fn construct_fast_path(
        &self,
        c: &ConstructQuery,
        layout: &VarLayout,
    ) -> Result<Option<Vec<Binding>>> {
        let Some(limit) = c.limit else {
            return Ok(None);
        };
        let k = c.offset.unwrap_or(0).saturating_add(limit);
        if c.order_by.is_empty() {
            if !self.topk.early_termination {
                return Ok(None);
            }
            return Ok(Some(self.first_solutions(&c.pattern, layout, k)));
        }
        if !self.topk.bounded_heap {
            return Ok(None);
        }
        if k == 0 {
            return Ok(Some(Vec::new()));
        }
        self.bump(|s| s.heap_topk += 1);
        let ctx = OrderCtx::new(layout, &c.order_by, None);
        Ok(Some(self.heap_top_k(&c.pattern, layout, &ctx, k)))
    }

    /// Extend a WHERE solution with the SELECT-expression columns the full
    /// path adds before sorting (see [`OrderCtx`]).
    fn order_extend(&self, ctx: &OrderCtx<'_>, b: &mut Binding) {
        for (e, lay) in &ctx.aliases {
            // The full path evaluates the expression before its column
            // exists; an unbound column reads the same (and a reference to
            // the alias itself cannot index past the end).
            b.push(None);
            let v = self.eval_id(e, b, lay);
            if let Some(slot) = b.last_mut() {
                *slot = v;
            }
        }
    }

    /// The sort key of an (extended) solution: one [`order_key`] per
    /// `ORDER BY` condition, descending ones reversed — compared
    /// lexicographically, as [`sort_solutions`](Self::sort_solutions) does.
    fn sort_key(
        &self,
        order_by: &[OrderCondition],
        b: &Binding,
        layout: &VarLayout,
    ) -> Vec<Dir<impl Ord>> {
        order_by
            .iter()
            .map(|cond| {
                let k = order_key(&self.eval_value(&cond.expr, b, layout));
                if cond.descending {
                    Dir::Desc(Reverse(k))
                } else {
                    Dir::Asc(k)
                }
            })
            .collect()
    }

    /// The `k` best solutions of `pattern` by `ctx`'s keys, ties in solution
    /// order, best first — the first `k` of the full path's stable sort.
    fn heap_top_k(
        &self,
        pattern: &GraphPattern,
        layout: &VarLayout,
        ctx: &OrderCtx<'_>,
        k: usize,
    ) -> Vec<Binding> {
        let mut heap = BinaryHeap::with_capacity(k.min(1024) + 1);
        let mut seq = 0u64;
        let _ = self.stream_pattern(pattern, layout, &mut |mut b| {
            self.order_extend(ctx, &mut b);
            let entry = Ranked {
                key: self.sort_key(ctx.order_by, &b, &ctx.full),
                seq,
                item: b,
            };
            seq += 1;
            if heap.len() < k {
                heap.push(entry);
            } else if let Some(mut worst) = heap.peek_mut() {
                if entry < *worst {
                    *worst = entry;
                }
            }
            GO
        });
        heap.into_sorted_vec().into_iter().map(|r| r.item).collect()
    }

    /// `SELECT DISTINCT … ORDER BY … LIMIT`: the `k` best distinct projected
    /// rows, each ranked by its best occurrence (key, then solution order),
    /// best first — what sorting, projecting and de-duplicating every
    /// solution and keeping `k` rows gives.
    fn heap_top_k_distinct(
        &self,
        q: &SelectQuery,
        pattern: &GraphPattern,
        layout: &VarLayout,
        ctx: &OrderCtx<'_>,
        result_vars: &[String],
        k: usize,
    ) -> Result<Vec<Vec<Option<u32>>>> {
        // best: (key, seq) → row, at most k entries; rank: row → its entry.
        let mut best = BTreeMap::new();
        let mut rank = HashMap::new();
        let mut seq = 0u64;
        let mut failed = None;
        let _ = self.stream_pattern(pattern, layout, &mut |mut b| {
            self.order_extend(ctx, &mut b);
            let key = std::rc::Rc::new(self.sort_key(ctx.order_by, &b, &ctx.full));
            let row = match self.project_items(&b, &ctx.full, q, result_vars) {
                Ok(row) => row,
                Err(e) => {
                    failed = Some(e);
                    return STOP;
                }
            };
            let this = (key, seq);
            seq += 1;
            match rank.get(&row) {
                // A later occurrence only matters with a strictly better key.
                Some(old) if this < *old => {
                    best.remove(old);
                    best.insert(this.clone(), row.clone());
                    rank.insert(row, this);
                }
                Some(_) => {}
                None if best.len() < k => {
                    best.insert(this.clone(), row.clone());
                    rank.insert(row, this);
                }
                None => {
                    if let Some(entry) = best.last_entry() {
                        if this < *entry.key() {
                            let evicted = entry.remove();
                            rank.remove(&evicted);
                            best.insert(this.clone(), row.clone());
                            rank.insert(row, this);
                        }
                    }
                }
            }
            GO
        });
        self.check_cancel()?;
        if let Some(e) = failed {
            return Err(e);
        }
        Ok(best.into_values().collect())
    }

    // ---- DP-B ------------------------------------------------------------

    /// DP-B Top-K (see the module docs): the matches of a tree-shaped BGP
    /// with the best first `ORDER BY` key — all those tied with the `k`-th —
    /// extended and sorted as the full path sorts. `None` when the query is
    /// outside what DP-B handles (the caller falls back to the heap).
    fn dpb_top_k(
        &self,
        q: &SelectQuery,
        pattern: &GraphPattern,
        layout: &VarLayout,
        ctx: &OrderCtx<'_>,
        result_vars: &[String],
        k: usize,
    ) -> Result<Option<Vec<Binding>>> {
        let (filters, tps): (&[Expr], &[TriplePattern]) = match pattern {
            GraphPattern::Bgp(tps) => (&[], tps),
            GraphPattern::Filter(fs, inner) => match inner.as_ref() {
                GraphPattern::Bgp(tps) => (fs, tps),
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };
        let Some(cond) = ctx.order_by.first() else {
            return Ok(None);
        };
        let Some(lin) = linear_key(&cond.expr, layout) else {
            return Ok(None);
        };
        if lin.coef.values().all(|&c| c == 0) {
            return Ok(None); // a constant key: every match ties
        }
        let sign: i128 = if cond.descending { -1 } else { 1 };
        let mut plans = Vec::with_capacity(tps.len());
        for tp in tps {
            match self.compile_pattern(tp, layout) {
                Some(p) => plans.push(p),
                None => return Ok(None),
            }
        }
        // A mixed variable (predicate and subject/object, see
        // `VarLayout::pred_alias`) links two slots in different id spaces,
        // which the DP-B tree cannot express: take the heap path.
        if !mixed_slots(&plans, layout).is_empty() {
            return Ok(None);
        }
        let Some(shape) = DpbShape::analyze(&plans, &lin) else {
            return Ok(None);
        };
        let Some(tree) = self.dpb_build(&plans, &shape, &lin, sign, layout.len()) else {
            return Ok(None);
        };
        self.check_cancel()?;

        // Pull matches in score order until k (distinct) rows are in, then
        // all matches tied with the k-th: a prefix of the sorted sequence.
        let cap = k.saturating_mul(8).saturating_add(10_000);
        let mut en = DpbEnum::new(&tree, self.cancel.as_deref());
        let mut out: Vec<Binding> = Vec::new();
        let mut counted = 0usize;
        let mut theta: Option<i128> = None;
        let mut seen = HashSet::new();
        let mut pulled = 0usize;
        let root = en.root();
        while !self.cancelled() {
            let Some(cost) = en.fr_get(root, pulled) else {
                break;
            };
            if theta.is_some_and(|t| cost > t) {
                break;
            }
            pulled += 1;
            if pulled > cap {
                self.bump(|s| s.dpb_matches += pulled as u64);
                return Ok(None);
            }
            let mut b = vec![None; layout.len()];
            en.fill_fr(root, pulled - 1, &mut b);
            if !filters
                .iter()
                .all(|f| self.eval_ebv(f, &b, layout) == Some(true))
            {
                continue;
            }
            self.order_extend(ctx, &mut b);
            if q.distinct {
                if seen.insert(self.project_items(&b, &ctx.full, q, result_vars)?) {
                    counted += 1;
                }
            } else {
                counted += 1;
            }
            out.push(b);
            if theta.is_none() && counted >= k {
                theta = Some(cost);
            }
        }
        self.bump(|s| s.dpb_matches += pulled as u64);
        self.check_cancel()?;

        // Order as the full path: by the keys, ties by the position of the
        // match in the BGP's solution sequence.
        let cons = if filters.is_empty() {
            HashMap::new()
        } else {
            self.filter_string_constraints(filters, layout)
        };
        let PreparedBgp::Plan(bgp) = self.prepare_bgp(tps, layout, &cons) else {
            return Ok(None);
        };
        let Some(positions) = self.solution_positions(&bgp, &out, layout.len()) else {
            return Ok(None);
        };
        let keys: Vec<_> = out
            .iter()
            .map(|b| self.sort_key(ctx.order_by, b, &ctx.full))
            .collect();
        let mut perm: Vec<usize> = (0..out.len()).collect();
        perm.sort_by(|&x, &y| {
            keys[x]
                .cmp(&keys[y])
                .then_with(|| positions[x].cmp(&positions[y]))
        });
        let mut slots: Vec<Option<Binding>> = out.into_iter().map(Some).collect();
        Ok(Some(perm.iter().filter_map(|&i| slots[i].take()).collect()))
    }

    /// The position of each BGP solution in the order the executor produces
    /// them, as a vector compared lexicographically: per left-deep step (or
    /// per leaf of a bushy plan, in tree order) the index of the solution's
    /// triple in that step's candidate list. `None` if a triple is missing
    /// from its list (not a solution of the plan).
    fn solution_positions(
        &self,
        bgp: &BgpPlan,
        sols: &[Binding],
        width: usize,
    ) -> Option<Vec<Vec<u32>>> {
        // The steps, each with the variables bound before it.
        let steps: Vec<(usize, HashSet<usize>)> = match &bgp.exec.tree {
            Some(tree) => {
                let mut leaves = Vec::new();
                tree_leaves(tree, &mut leaves);
                leaves.into_iter().map(|i| (i, HashSet::new())).collect()
            }
            None => {
                let mut bound = HashSet::new();
                let mut steps = Vec::new();
                for &i in &bgp.exec.order {
                    steps.push((i, bound.clone()));
                    bound.extend(optimizer::plan_vars(&bgp.plans[i]));
                }
                steps
            }
        };
        type Lookup = (usize, Option<u32>, Option<u32>, Option<u32>);
        let known = |slot: &Slot, b: &Binding, bound: &HashSet<usize>| match slot {
            Slot::Const(c) => Some(*c),
            Slot::Var(v) if bound.contains(v) => b[*v],
            Slot::Var(_) => None,
        };
        let value = |slot: &Slot, b: &Binding| match slot {
            Slot::Const(c) => Some(*c),
            Slot::Var(v) => b[*v],
        };
        // Which triples each candidate list must place.
        let mut needs: HashMap<Lookup, HashMap<IdTriple3, u32>> = HashMap::new();
        let mut per_sol: Vec<Vec<(Lookup, IdTriple3)>> = Vec::with_capacity(sols.len());
        for b in sols {
            let mut keys = Vec::with_capacity(steps.len());
            for (t, (pi, bound)) in steps.iter().enumerate() {
                let plan = &bgp.plans[*pi];
                let triple = (value(&plan.s, b)?, value(&plan.p, b)?, value(&plan.o, b)?);
                let lookup = (
                    t,
                    known(&plan.s, b, bound),
                    known(&plan.p, b, bound),
                    known(&plan.o, b, bound),
                );
                needs.entry(lookup).or_default().insert(triple, u32::MAX);
                keys.push((lookup, triple));
            }
            per_sol.push(keys);
        }
        for (lookup, wanted) in needs.iter_mut() {
            let (t, ks, kp, ko) = *lookup;
            let plan = &bgp.plans[steps[t].0];
            let mut probe = vec![None; width];
            for (slot, known) in [(&plan.s, ks), (&plan.p, kp), (&plan.o, ko)] {
                if let (Slot::Var(v), Some(id)) = (slot, known) {
                    probe[*v] = Some(id);
                }
            }
            for (i, triple) in self.candidate_triples(&probe, plan).into_iter().enumerate() {
                if let Some(pos) = wanted.get_mut(&triple) {
                    if *pos == u32::MAX {
                        *pos = u32::try_from(i).unwrap_or(u32::MAX - 1);
                    }
                }
            }
        }
        per_sol
            .into_iter()
            .map(|keys| {
                keys.into_iter()
                    .map(|(lookup, triple)| {
                        let pos = needs[&lookup][&triple];
                        (pos != u32::MAX).then_some(pos)
                    })
                    .collect()
            })
            .collect()
    }

    /// Build the DP-B search tree: candidate values per variable (top down
    /// from the root, through the tree edges), the semi-join reduction
    /// bottom up, then the scores of the values a match can use. `None` when
    /// a scored variable can take a non-integer value.
    fn dpb_build(
        &self,
        plans: &[PatPlan],
        shape: &DpbShape,
        lin: &LinearKey,
        sign: i128,
        width: usize,
    ) -> Option<DpbTree> {
        // Unary restrictions.
        let mut unary: HashMap<usize, Vec<u32>> = HashMap::new();
        for (&v, pats) in &shape.unary {
            let mut acc: Option<Vec<u32>> = None;
            for &pi in pats {
                let plan = &plans[pi];
                let Slot::Const(p) = plan.p else {
                    return None;
                };
                let mut vals: Vec<u32> = match (plan.s, plan.o) {
                    (Slot::Var(_), Slot::Const(o)) => self.store.s_by_po(p, o),
                    (Slot::Const(s), Slot::Var(_)) => self.store.o_by_sp(s, p),
                    (Slot::Var(_), Slot::Var(_)) => self
                        .store
                        .so_by_p(p)
                        .into_iter()
                        .filter(|(s, o)| s == o)
                        .map(|(s, _)| s)
                        .collect(),
                    _ => return None,
                };
                vals.sort_unstable();
                vals.dedup();
                acc = Some(match acc {
                    None => vals,
                    Some(prev) => prev
                        .into_iter()
                        .filter(|x| vals.binary_search(x).is_ok())
                        .collect(),
                });
            }
            unary.insert(v, acc.unwrap_or_default());
        }

        // Root: the most selective variable.
        let mut best: Option<(usize, usize)> = None; // (estimate, var)
        for &v in &shape.nodes {
            let est = match unary.get(&v) {
                Some(vals) => vals.len(),
                None => shape
                    .incident(v, plans)
                    .map(|(pi, subject)| match plans[pi].p {
                        Slot::Const(p) if subject => self.store.pred_distinct_subj(p),
                        Slot::Const(p) => self.store.pred_distinct_obj(p),
                        Slot::Var(_) if subject => self.store.distinct_subjects(),
                        Slot::Var(_) => self.store.distinct_objects(),
                    })
                    .min()
                    .unwrap_or(usize::MAX),
            };
            if best.is_none_or(|(e, _)| est < e) {
                best = Some((est, v));
            }
        }
        let (_, root) = best?;
        let mut root_vals: Vec<u32> = match unary.get(&root) {
            Some(vals) => vals.clone(),
            None => {
                let (pi, subject) =
                    shape
                        .incident(root, plans)
                        .min_by_key(|&(pi, subject)| match plans[pi].p {
                            Slot::Const(p) if subject => self.store.pred_distinct_subj(p),
                            Slot::Const(p) => self.store.pred_distinct_obj(p),
                            Slot::Var(_) if subject => self.store.distinct_subjects(),
                            Slot::Var(_) => self.store.distinct_objects(),
                        })?;
                match plans[pi].p {
                    Slot::Const(p) if subject => self.store.subs_by_p(p),
                    Slot::Const(p) => self.store.objs_by_p(p),
                    Slot::Var(_) if subject => self.store.subject_keys(),
                    Slot::Var(_) => self.store.object_keys(),
                }
            }
        };
        root_vals.sort_unstable();
        root_vals.dedup();

        // Orient the tree from the root (breadth first).
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); width];
        let mut edge_pats: Vec<Vec<(usize, bool)>> = vec![Vec::new(); width];
        let mut edge_pred_vars: Vec<Vec<usize>> = vec![Vec::new(); width];
        let mut bfs = vec![root];
        let mut seen: HashSet<usize> = HashSet::from([root]);
        let mut at = 0;
        while let Some(&p) = bfs.get(at) {
            at += 1;
            for (&(a, b), pats) in &shape.edges {
                let c = match (a == p, b == p) {
                    (true, false) => b,
                    (false, true) => a,
                    _ => continue,
                };
                if !seen.insert(c) {
                    continue;
                }
                children[p].push(c);
                // Constant predicates first: they narrow the children.
                let mut oriented: Vec<(usize, bool)> = pats
                    .iter()
                    .map(|&pi| (pi, plans[pi].s.var() == Some(p)))
                    .collect();
                oriented.sort_by_key(|&(pi, _)| (matches!(plans[pi].p, Slot::Var(_)), pi));
                edge_pred_vars[c] = oriented
                    .iter()
                    .filter_map(|&(pi, _)| plans[pi].p.var())
                    .collect();
                edge_pats[c] = oriented;
                bfs.push(c);
            }
        }
        for ch in &mut children {
            ch.sort_unstable();
        }

        // Worth it? Estimate the matches (root values times each tree
        // edge's average fan-out) against the values DP-B reads.
        if self.topk.dpb == DpbMode::Auto {
            let mut fanout_cache = HashMap::new();
            let mut values: HashMap<usize, f64> = HashMap::from([(root, root_vals.len() as f64)]);
            let (mut matches, mut input) = (root_vals.len() as f64, root_vals.len() as f64);
            for &p in &bfs {
                for &c in &children[p] {
                    let f = self.dpb_fanout(&edge_pats[c], plans, unary.get(&c), &mut fanout_cache);
                    let n = values[&p] * f;
                    values.insert(c, n);
                    matches *= f;
                    input += n;
                }
            }
            if matches < DPB_MIN_GAIN * input {
                return None;
            }
        }

        // Top down: each parent value's children along each tree edge.
        let mut cand: HashMap<usize, Vec<u32>> = HashMap::from([(root, root_vals)]);
        let mut lists: EdgeLists = vec![HashMap::new(); width];
        for &p in &bfs {
            for &c in &children[p] {
                let mut child_vals = BTreeSet::new();
                let mut list = HashMap::new();
                for (n, &u) in cand
                    .get(&p)
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                    .iter()
                    .enumerate()
                {
                    if n % 1024 == 0 && self.cancelled() {
                        return None;
                    }
                    let entries = self.dpb_edge_entries(u, &edge_pats[c], plans, unary.get(&c));
                    if !entries.is_empty() {
                        child_vals.extend(entries.iter().map(|(w, _)| *w));
                        list.insert(u, entries);
                    }
                }
                cand.insert(c, child_vals.into_iter().collect());
                lists[c] = list;
            }
        }

        // Bottom up: a value is valid when every child edge keeps a valid
        // child.
        let mut valid: HashMap<usize, HashSet<u32>> = HashMap::new();
        for &v in bfs.iter().rev() {
            let mut ok = HashSet::new();
            'value: for &w in cand.get(&v).map(Vec::as_slice).unwrap_or(&[]) {
                for &c in &children[v] {
                    let vc = &valid[&c];
                    match lists[c].get_mut(&w) {
                        Some(list) => {
                            list.retain(|(x, _)| vc.contains(x));
                            if list.is_empty() {
                                continue 'value;
                            }
                        }
                        None => continue 'value,
                    }
                }
                ok.insert(w);
            }
            valid.insert(v, ok);
        }
        let mut roots: Vec<u32> = valid[&root].iter().copied().collect();
        roots.sort_unstable();

        // Top down again: the values some match uses, and their scores.
        let mut reach: HashMap<usize, BTreeSet<u32>> =
            HashMap::from([(root, roots.iter().copied().collect())]);
        for &p in &bfs {
            for &c in &children[p] {
                let mut r = BTreeSet::new();
                for u in &reach[&p] {
                    if let Some(list) = lists[c].get(u) {
                        r.extend(list.iter().map(|(x, _)| *x));
                    }
                }
                reach.insert(c, r);
            }
        }
        let mut score: Vec<HashMap<u32, i128>> = vec![HashMap::new(); width];
        for &v in &lin.vars {
            let c = sign * lin.coef.get(&v).copied().unwrap_or(0);
            for &x in reach.get(&v).into_iter().flatten() {
                match self.id_to_value(x, false) {
                    Some(Value::Int(i)) => {
                        score[v].insert(x, c * i as i128);
                    }
                    _ => return None,
                }
            }
        }

        // Bottom up: the best score of each value's subtree — the first
        // element of its FQ list, known before the list exists.
        let mut best: Vec<HashMap<u32, i128>> = vec![HashMap::new(); width];
        for &v in bfs.iter().rev() {
            let mut b = HashMap::new();
            for &x in reach.get(&v).into_iter().flatten() {
                let mut total = score[v].get(&x).copied().unwrap_or(0);
                for &c in &children[v] {
                    let entries = lists[c].get(&x).map(Vec::as_slice).unwrap_or(&[]);
                    total += entries
                        .iter()
                        .filter_map(|(y, _)| best[c].get(y).copied())
                        .min()?;
                }
                b.insert(x, total);
            }
            best[v] = b;
        }
        Some(DpbTree {
            root,
            roots,
            children,
            lists,
            edge_pred_vars,
            score,
            best,
        })
    }

    /// The average number of children a parent value has along one tree
    /// edge, from the store's statistics: the most selective of its
    /// patterns, scaled down by the child's restrictions.
    fn dpb_fanout(
        &self,
        pats: &[(usize, bool)],
        plans: &[PatPlan],
        unary: Option<&Vec<u32>>,
        cache: &mut HashMap<Option<u32>, (f64, f64, f64)>,
    ) -> f64 {
        let mut fanout = f64::INFINITY;
        let mut child_domain = f64::INFINITY;
        for &(pi, fwd) in pats {
            // (triples, distinct subjects, distinct objects) of the predicate
            // (`None`: a predicate variable — the whole store).
            let pred = match plans[pi].p {
                Slot::Const(p) => Some(p),
                Slot::Var(_) => None,
            };
            let (card, subj, obj) = *cache.entry(pred).or_insert_with(|| match pred {
                Some(p) => (
                    self.store.pred_card(p) as f64,
                    self.store.pred_distinct_subj(p) as f64,
                    self.store.pred_distinct_obj(p) as f64,
                ),
                None => (
                    self.store.triple_count() as f64,
                    self.store.distinct_subjects() as f64,
                    self.store.distinct_objects() as f64,
                ),
            });
            let (parents, children) = if fwd { (subj, obj) } else { (obj, subj) };
            fanout = fanout.min(card / parents.max(1.0));
            child_domain = child_domain.min(children.max(1.0));
        }
        if let Some(un) = unary {
            fanout *= (un.len() as f64 / child_domain).min(1.0);
        }
        if fanout.is_finite() {
            fanout
        } else {
            1.0
        }
    }

    /// The children of parent value `u` along one tree edge (`pats`: the
    /// edge's patterns, constant predicates first, each with whether the
    /// parent is its subject): each child value with one entry per
    /// combination of the edge's predicate-variable values.
    fn dpb_edge_entries(
        &self,
        u: u32,
        pats: &[(usize, bool)],
        plans: &[PatPlan],
        unary: Option<&Vec<u32>>,
    ) -> Vec<EdgeEntry> {
        let mut base: Option<Vec<u32>> = None;
        let mut var_pats = Vec::new();
        for &(pi, fwd) in pats {
            let Slot::Const(p) = plans[pi].p else {
                var_pats.push((pi, fwd));
                continue;
            };
            let next = match base {
                None => {
                    let mut ws = if fwd {
                        self.store.o_by_sp(u, p)
                    } else {
                        self.store.s_by_po(p, u)
                    };
                    ws.sort_unstable();
                    ws.dedup();
                    ws
                }
                Some(ws) => ws
                    .into_iter()
                    .filter(|&w| {
                        if fwd {
                            self.store.exists(u, p, w)
                        } else {
                            self.store.exists(w, p, u)
                        }
                    })
                    .collect(),
            };
            if next.is_empty() {
                return Vec::new();
            }
            base = Some(next);
        }
        let ws = match base {
            Some(ws) => ws,
            None => {
                let Some(&(_, fwd)) = var_pats.first() else {
                    return Vec::new();
                };
                let mut ws: Vec<u32> = if fwd {
                    self.store.po_by_s(u).into_iter().map(|(_, o)| o).collect()
                } else {
                    self.store.ps_by_o(u).into_iter().map(|(_, s)| s).collect()
                };
                ws.sort_unstable();
                ws.dedup();
                ws
            }
        };
        let mut out = Vec::new();
        for w in ws {
            if unary.is_some_and(|un| un.binary_search(&w).is_err()) {
                continue;
            }
            let mut combos: Vec<Vec<u32>> = vec![Vec::new()];
            for &(_, fwd) in &var_pats {
                let mut ps = if fwd {
                    self.store.p_by_so(u, w)
                } else {
                    self.store.p_by_so(w, u)
                };
                ps.sort_unstable();
                ps.dedup();
                combos = combos
                    .into_iter()
                    .flat_map(|c| {
                        ps.iter().map(move |&p| {
                            let mut c = c.clone();
                            c.push(p);
                            c
                        })
                    })
                    .collect();
            }
            out.extend(combos.into_iter().map(|c| (w, c)));
        }
        out
    }
}

/// Clone a [`VarLayout`].
fn clone_layout(l: &VarLayout) -> VarLayout {
    VarLayout {
        index: l.index.clone(),
        names: l.names.clone(),
        is_pred: l.is_pred.clone(),
        pred_alias: l.pred_alias.clone(),
    }
}

/// The variables a bushy-plan subtree binds (as
/// [`Evaluator::eval_join_tree`] accumulates them).
fn tree_vars(tree: &JoinTree, plans: &[PatPlan]) -> Vec<usize> {
    match tree {
        JoinTree::Leaf(i) => optimizer::plan_vars(&plans[*i]),
        JoinTree::Join(l, r) => {
            let mut vars = tree_vars(l, plans);
            for v in tree_vars(r, plans) {
                if !vars.contains(&v) {
                    vars.push(v);
                }
            }
            vars
        }
    }
}

/// The leaves of a bushy plan, left to right.
fn tree_leaves(tree: &JoinTree, out: &mut Vec<usize>) {
    match tree {
        JoinTree::Leaf(i) => out.push(*i),
        JoinTree::Join(l, r) => {
            tree_leaves(l, out);
            tree_leaves(r, out);
        }
    }
}

/// One materialized join operand, indexed for probing: yields, for a left
/// solution, every compatible right solution merged with it, in the right
/// operand's order — what [`join_sets`] pairs it with.
struct Probe {
    rows: Vec<Binding>,
    /// Shared variables bound in every row (the hash key).
    key: Vec<usize>,
    index: HashMap<Vec<u32>, Vec<usize>>,
}

impl Probe {
    /// Index `rows` on the variables of `hint` (the shared variables) that
    /// every row binds. Which variables are used only affects speed: a left
    /// solution that leaves one of them unbound is matched by a scan.
    fn new(rows: Vec<Binding>, hint: &[usize]) -> Probe {
        let key: Vec<usize> = hint
            .iter()
            .copied()
            .filter(|&i| rows.iter().all(|r| r[i].is_some()))
            .collect();
        let mut index: HashMap<Vec<u32>, Vec<usize>> = HashMap::new();
        if !key.is_empty() {
            for (ri, r) in rows.iter().enumerate() {
                let k = key.iter().map(|&i| r[i].unwrap_or(u32::MAX)).collect();
                index.entry(k).or_default().push(ri);
            }
        }
        Probe { rows, key, index }
    }

    fn for_each_compatible(&self, l: &Binding, f: &mut Sink<'_>) -> Flow {
        if !self.key.is_empty() {
            let k: Option<Vec<u32>> = self.key.iter().map(|&i| l[i]).collect();
            if let Some(k) = k {
                for &ri in self.index.get(&k).map(Vec::as_slice).unwrap_or(&[]) {
                    if let Some(m) = merge_bindings(l, &self.rows[ri]) {
                        f(m)?;
                    }
                }
                return GO;
            }
        }
        for r in &self.rows {
            if let Some(m) = merge_bindings(l, r) {
                f(m)?;
            }
        }
        GO
    }
}

/// Join `b` with each probe in turn, depth first.
fn join_dfs(probes: &[Probe], b: Binding, sink: &mut Sink<'_>) -> Flow {
    match probes.split_first() {
        None => sink(b),
        Some((p, rest)) => p.for_each_compatible(&b, &mut |m| join_dfs(rest, m, sink)),
    }
}

/// The columns and layouts the full path's `ORDER BY` uses: SELECT
/// expressions whose alias is not a pattern variable become columns, added
/// one at a time — each evaluated in the layout that has the aliases up to
/// its own — then the keys are evaluated in the layout with all of them.
struct OrderCtx<'q> {
    order_by: &'q [OrderCondition],
    aliases: Vec<(&'q Expr, VarLayout)>,
    full: VarLayout,
}

impl<'q> OrderCtx<'q> {
    fn new(
        layout: &VarLayout,
        order_by: &'q [OrderCondition],
        projection: Option<&'q Projection>,
    ) -> OrderCtx<'q> {
        let mut full = clone_layout(layout);
        let mut aliases = Vec::new();
        if let Some(Projection::Items(items)) = projection {
            for it in items {
                if let SelectItem::Expr(e, alias) = it {
                    if !full.index.contains_key(alias) {
                        full.index.insert(alias.clone(), full.names.len());
                        full.names.push(alias.clone());
                        full.is_pred.push(false);
                        aliases.push((e, clone_layout(&full)));
                    }
                }
            }
        }
        OrderCtx {
            order_by,
            aliases,
            full,
        }
    }
}

/// One `ORDER BY` key, in its direction.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Dir<K> {
    Asc(K),
    Desc(Reverse<K>),
}

/// A heap entry: ordered by key, then by arrival (solution order).
struct Ranked<K, T> {
    key: K,
    seq: u64,
    item: T,
}

impl<K: Ord, T> PartialEq for Ranked<K, T> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl<K: Ord, T> Eq for Ranked<K, T> {}
impl<K: Ord, T> PartialOrd for Ranked<K, T> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl<K: Ord, T> Ord for Ranked<K, T> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key
            .cmp(&other.key)
            .then_with(|| self.seq.cmp(&other.seq))
    }
}

// ---- DP-B ----------------------------------------------------------------

/// An `(s, p, o)` triple of ids.
type IdTriple3 = (u32, u32, u32);
/// A tree-edge entry: a child value and the values of the edge's predicate
/// variables.
type EdgeEntry = (u32, Vec<u32>);
/// Child variable → parent value → its entries along the tree edge.
type EdgeLists = Vec<HashMap<u32, Vec<EdgeEntry>>>;

/// Largest `Σ|cᵢ|` of a linear key (and of each of its subexpressions):
/// with `i64` values every intermediate result stays far inside `i128`, so
/// SPARQL's exact integer arithmetic and the scores agree.
const MAX_COEF_SUM: i128 = 1 << 32;
/// Largest constant term.
const MAX_CONST: i128 = 1 << 62;
/// [`DpbMode::Auto`] runs DP-B when it expects at least this many matches
/// per value it reads.
const DPB_MIN_GAIN: f64 = 2.0;

/// `Σ coef[v]·?v + constant` with integer coefficients; `vars` are all the
/// variables it reads (a zero coefficient still needs an integer value, or
/// SPARQL's evaluation errs).
#[derive(Debug, Clone, Default)]
struct LinearKey {
    coef: BTreeMap<usize, i128>,
    vars: BTreeSet<usize>,
    constant: i128,
}

impl LinearKey {
    fn scaled(mut self, by: i128) -> Option<LinearKey> {
        for c in self.coef.values_mut() {
            *c = c.checked_mul(by)?;
        }
        self.constant = self.constant.checked_mul(by)?;
        Some(self)
    }

    fn plus(mut self, other: LinearKey) -> Option<LinearKey> {
        for (v, c) in other.coef {
            let e = self.coef.entry(v).or_insert(0);
            *e = e.checked_add(c)?;
        }
        self.vars.extend(other.vars);
        self.constant = self.constant.checked_add(other.constant)?;
        Some(self)
    }

    fn in_bounds(&self) -> bool {
        let sum = self
            .coef
            .values()
            .try_fold(0i128, |acc, c| acc.checked_add(c.checked_abs()?));
        sum.is_some_and(|s| s <= MAX_COEF_SUM) && self.constant.abs() <= MAX_CONST
    }
}

/// The linear form of an `ORDER BY` key, if it is one DP-B can rank
/// exactly (see the module docs).
fn linear_key(e: &Expr, layout: &VarLayout) -> Option<LinearKey> {
    let lk = match e {
        Expr::Var(v) => {
            let i = *layout.index.get(v)?;
            LinearKey {
                coef: BTreeMap::from([(i, 1)]),
                vars: BTreeSet::from([i]),
                constant: 0,
            }
        }
        Expr::Const(t) => match Value::from_term(t) {
            Value::Int(i) => LinearKey {
                constant: i as i128,
                ..LinearKey::default()
            },
            _ => return None,
        },
        Expr::Unary(UnaryOp::Plus, x) => linear_key(x, layout)?,
        Expr::Unary(UnaryOp::Neg, x) => linear_key(x, layout)?.scaled(-1)?,
        Expr::Arith(ArithOp::Add, x, y) => linear_key(x, layout)?.plus(linear_key(y, layout)?)?,
        Expr::Arith(ArithOp::Sub, x, y) => {
            linear_key(x, layout)?.plus(linear_key(y, layout)?.scaled(-1)?)?
        }
        Expr::Arith(ArithOp::Mul, x, y) => {
            let (a, b) = (linear_key(x, layout)?, linear_key(y, layout)?);
            if a.vars.is_empty() {
                let mut r = b.scaled(a.constant)?;
                r.vars.extend(a.vars);
                r
            } else if b.vars.is_empty() {
                a.scaled(b.constant)?
            } else {
                return None;
            }
        }
        _ => return None,
    };
    lk.in_bounds().then_some(lk)
}

/// The query graph of a BGP, if DP-B handles it: subject/object variables
/// (`nodes`) joined by tree edges, plus per-variable restrictions.
struct DpbShape {
    nodes: BTreeSet<usize>,
    /// `(a, b)` with `a < b` → the patterns between those two variables.
    edges: BTreeMap<(usize, usize), Vec<usize>>,
    /// Variable → patterns that only restrict it.
    unary: BTreeMap<usize, Vec<usize>>,
}

impl DpbShape {
    fn analyze(plans: &[PatPlan], lin: &LinearKey) -> Option<DpbShape> {
        let mut nodes = BTreeSet::new();
        for p in plans {
            nodes.extend(p.s.var());
            nodes.extend(p.o.var());
        }
        let mut pred_vars = HashSet::new();
        let mut edges: BTreeMap<(usize, usize), Vec<usize>> = BTreeMap::new();
        let mut unary: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (i, p) in plans.iter().enumerate() {
            if let Some(pv) = p.p.var() {
                // A predicate variable: in one pattern only, never a node.
                if nodes.contains(&pv) || !pred_vars.insert(pv) {
                    return None;
                }
            }
            match (p.s.var(), p.o.var()) {
                (Some(a), Some(b)) if a != b => {
                    edges.entry((a.min(b), a.max(b))).or_default().push(i);
                }
                (Some(a), Some(_)) | (Some(a), None) | (None, Some(a)) => {
                    if p.p.var().is_some() {
                        return None;
                    }
                    unary.entry(a).or_default().push(i);
                }
                // A ground pattern: let the general path check it.
                (None, None) => return None,
            }
        }
        // Every key variable is a node; the nodes form one tree.
        if nodes.is_empty() || !lin.vars.iter().all(|v| nodes.contains(v)) {
            return None;
        }
        if edges.len() + 1 != nodes.len() {
            return None;
        }
        let first = *nodes.first()?;
        let mut reached = BTreeSet::from([first]);
        let mut stack = vec![first];
        while let Some(v) = stack.pop() {
            for &(a, b) in edges.keys() {
                let other = if a == v {
                    b
                } else if b == v {
                    a
                } else {
                    continue;
                };
                if reached.insert(other) {
                    stack.push(other);
                }
            }
        }
        (reached.len() == nodes.len()).then_some(DpbShape {
            nodes,
            edges,
            unary,
        })
    }

    /// The patterns of the tree edges at `v`, each with whether `v` is the
    /// subject.
    fn incident<'s>(
        &'s self,
        v: usize,
        plans: &'s [PatPlan],
    ) -> impl Iterator<Item = (usize, bool)> + 's {
        self.edges
            .iter()
            .filter(move |((a, b), _)| *a == v || *b == v)
            .flat_map(|(_, pats)| pats.iter().copied())
            .map(move |pi| (pi, plans[pi].s.var() == Some(v)))
    }
}

/// The reduced DP-B search tree ([`Evaluator::dpb_build`]).
struct DpbTree {
    root: usize,
    /// Valid root values.
    roots: Vec<u32>,
    /// Variable → its child variables.
    children: Vec<Vec<usize>>,
    /// Child variable → parent value → (child value, predicate-variable
    /// values) entries, reduced to valid children.
    lists: EdgeLists,
    /// Child variable → the predicate variables of its tree edge.
    edge_pred_vars: Vec<Vec<usize>>,
    /// Variable → value → its share of the score (absent: 0).
    score: Vec<HashMap<u32, i128>>,
    /// Variable → value → the best score of its subtree's matches.
    best: Vec<HashMap<u32, i128>>,
}

/// An `FQ` list: the matches of the subtree at one variable value, by score.
struct Fq {
    var: usize,
    value: u32,
    state: FqState,
    /// One `FR` list per child variable.
    children: Vec<usize>,
    /// The computed elements: score and an index into each child list.
    pool: Vec<(i128, Vec<u32>)>,
    /// Frontier: score, child indices, first coordinate it may increment.
    heap: BinaryHeap<Reverse<(i128, Vec<u32>, usize)>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FqState {
    New,
    Running,
    Done,
}

/// An `FR` list: the `FQ` lists of the children of one parent value along
/// one tree edge (or of every root value), merged by score.
struct Fr {
    child_var: usize,
    /// `None` for the root list.
    parent: Option<u32>,
    /// The child values whose `FQ` lists are merged; an `FQ` list is only
    /// created once its first element is taken (until then its best score
    /// stands in for it).
    members: Vec<u32>,
    started: bool,
    /// The computed elements: score, member, index into the member's list.
    pool: Vec<(i128, u32, u32)>,
    heap: BinaryHeap<Reverse<(i128, u32, u32)>>,
}

/// Lazy best-first enumeration over a [`DpbTree`].
struct DpbEnum<'t> {
    t: &'t DpbTree,
    cancel: Option<&'t std::sync::atomic::AtomicBool>,
    fq_ids: HashMap<(usize, u32), usize>,
    fqs: Vec<Fq>,
    frs: Vec<Fr>,
}

impl<'t> DpbEnum<'t> {
    fn new(t: &'t DpbTree, cancel: Option<&'t std::sync::atomic::AtomicBool>) -> DpbEnum<'t> {
        DpbEnum {
            t,
            cancel,
            fq_ids: HashMap::new(),
            fqs: Vec::new(),
            frs: Vec::new(),
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel
            .is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// The root `FR` list (every valid root value).
    fn root(&mut self) -> usize {
        self.frs.push(Fr {
            child_var: self.t.root,
            parent: None,
            members: self.t.roots.clone(),
            started: false,
            pool: Vec::new(),
            heap: BinaryHeap::new(),
        });
        self.frs.len() - 1
    }

    fn fq_id(&mut self, var: usize, value: u32) -> usize {
        if let Some(&id) = self.fq_ids.get(&(var, value)) {
            return id;
        }
        self.fqs.push(Fq {
            var,
            value,
            state: FqState::New,
            children: Vec::new(),
            pool: Vec::new(),
            heap: BinaryHeap::new(),
        });
        let id = self.fqs.len() - 1;
        self.fq_ids.insert((var, value), id);
        id
    }

    /// The `FR` list of parent value `parent` along the edge to `child_var`.
    fn new_fr(&mut self, child_var: usize, parent: u32) -> usize {
        let members = self.t.lists[child_var]
            .get(&parent)
            .map(Vec::as_slice)
            .unwrap_or(&[])
            .iter()
            .map(|&(x, _)| x)
            .collect();
        self.frs.push(Fr {
            child_var,
            parent: Some(parent),
            members,
            started: false,
            pool: Vec::new(),
            heap: BinaryHeap::new(),
        });
        self.frs.len() - 1
    }

    /// The score of the `i`-th best element of an `FR` list.
    fn fr_get(&mut self, id: usize, i: usize) -> Option<i128> {
        loop {
            if let Some(&(c, _, _)) = self.frs[id].pool.get(i) {
                return Some(c);
            }
            if !self.fr_step(id) {
                return None;
            }
        }
    }

    fn fr_step(&mut self, id: usize) -> bool {
        if !self.frs[id].started {
            if self.cancelled() {
                return false;
            }
            let fr = &mut self.frs[id];
            fr.started = true;
            let best = &self.t.best[fr.child_var];
            let first: Vec<Reverse<(i128, u32, u32)>> = fr
                .members
                .iter()
                .enumerate()
                .filter_map(|(m, x)| Some(Reverse((*best.get(x)?, m as u32, 0))))
                .collect();
            fr.heap = BinaryHeap::from(first);
        }
        let Some(Reverse((c, m, s))) = self.frs[id].heap.pop() else {
            return false;
        };
        self.frs[id].pool.push((c, m, s));
        let (var, x) = (self.frs[id].child_var, self.frs[id].members[m as usize]);
        let fq = self.fq_id(var, x);
        if let Some(next) = self.fq_get(fq, s as usize + 1) {
            self.frs[id].heap.push(Reverse((next, m, s + 1)));
        }
        true
    }

    /// The score of the `i`-th best element of an `FQ` list.
    fn fq_get(&mut self, id: usize, i: usize) -> Option<i128> {
        loop {
            if let Some((c, _)) = self.fqs[id].pool.get(i) {
                return Some(*c);
            }
            if !self.fq_step(id) {
                return None;
            }
        }
    }

    fn fq_step(&mut self, id: usize) -> bool {
        match self.fqs[id].state {
            FqState::Done => return false,
            FqState::Running => {}
            FqState::New => {
                let (var, value) = (self.fqs[id].var, self.fqs[id].value);
                let mut cost = self.t.score[var].get(&value).copied().unwrap_or(0);
                let mut frs = Vec::with_capacity(self.t.children[var].len());
                for &c in &self.t.children[var] {
                    frs.push(self.new_fr(c, value));
                }
                for &fr in &frs {
                    match self.fr_get(fr, 0) {
                        Some(c) => cost += c,
                        None => {
                            self.fqs[id].state = FqState::Done;
                            return false;
                        }
                    }
                }
                let n = frs.len();
                self.fqs[id].children = frs;
                self.fqs[id].heap.push(Reverse((cost, vec![0; n], 0)));
                self.fqs[id].state = FqState::Running;
            }
        }
        let Some(Reverse((cost, idx, last))) = self.fqs[id].heap.pop() else {
            self.fqs[id].state = FqState::Done;
            return false;
        };
        // Successors: increment one coordinate at or after the last one
        // incremented, so every index vector is reached exactly once.
        for j in last..idx.len() {
            let fr = self.fqs[id].children[j];
            let cur = self.frs[fr].pool[idx[j] as usize].0;
            if let Some(next) = self.fr_get(fr, idx[j] as usize + 1) {
                let mut succ = idx.clone();
                succ[j] += 1;
                self.fqs[id]
                    .heap
                    .push(Reverse((cost - cur + next, succ, j)));
            }
        }
        self.fqs[id].pool.push((cost, idx));
        true
    }

    /// Write the `i`-th element of an `FR` list into `b`.
    fn fill_fr(&self, id: usize, i: usize, b: &mut Binding) {
        let fr = &self.frs[id];
        let (_, m, s) = fr.pool[i];
        if let Some(parent) = fr.parent {
            if let Some((_, preds)) = self.t.lists[fr.child_var]
                .get(&parent)
                .and_then(|l| l.get(m as usize))
            {
                for (&pv, &pid) in self.t.edge_pred_vars[fr.child_var].iter().zip(preds) {
                    b[pv] = Some(pid);
                }
            }
        }
        if let Some(&fq) = self.fq_ids.get(&(fr.child_var, fr.members[m as usize])) {
            self.fill_fq(fq, s as usize, b);
        }
    }

    fn fill_fq(&self, id: usize, i: usize, b: &mut Binding) {
        let fq = &self.fqs[id];
        b[fq.var] = Some(fq.value);
        let idx = &fq.pool[i].1;
        for (j, &fr) in fq.children.iter().enumerate() {
            self.fill_fr(fr, idx[j] as usize, b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::IdTriple;
    use crate::parser::sparql::parse;

    /// SplitMix64.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    /// Random `:p0`..`:p2` edges over `n` entities.
    fn graph(n: usize, edges: usize, seed: u64) -> (Dictionary, TripleStore) {
        let mut rng = Rng(seed);
        let mut d = Dictionary::new();
        let ents: Vec<u32> = (0..n)
            .map(|i| d.intern_term(&Term::iri(format!("http://ex/e{i}"))))
            .collect();
        let preds: Vec<u32> = (0..3)
            .map(|i| d.intern_predicate(&Term::iri(format!("http://ex/p{i}")).dict_key()))
            .collect();
        let mut triples = Vec::new();
        for _ in 0..edges {
            triples.push(IdTriple::new(
                ents[rng.below(n)],
                preds[rng.below(3)],
                ents[rng.below(n)],
            ));
        }
        for &p in &preds {
            triples.push(IdTriple::new(ents[0], p, ents[1]));
        }
        let mut s = TripleStore::new();
        s.bulk_load(triples);
        (d, s)
    }

    fn bgp(body: &str) -> GraphPattern {
        let q = parse(&format!(
            "PREFIX : <http://ex/> SELECT * WHERE {{ {body} }}"
        ))
        .unwrap();
        match q {
            Query::Select(s) => s.pattern,
            other => panic!("{other:?}"),
        }
    }

    fn compiled<S: TripleSource>(
        ev: &Evaluator<'_, S>,
        gp: &GraphPattern,
        layout: &VarLayout,
    ) -> (Vec<PatPlan>, Candidates) {
        let GraphPattern::Bgp(tps) = gp else {
            panic!("not a BGP")
        };
        let plans: Vec<PatPlan> = tps
            .iter()
            .map(|tp| ev.compile_pattern(tp, layout).unwrap())
            .collect();
        let cands = candidates::generate(&plans, ev.store);
        (plans, cands)
    }

    fn leaf(i: usize) -> JoinTree {
        JoinTree::Leaf(i)
    }
    fn join(l: JoinTree, r: JoinTree) -> JoinTree {
        JoinTree::Join(Box::new(l), Box::new(r))
    }

    fn copy_plans(plans: &[PatPlan]) -> Vec<PatPlan> {
        plans
            .iter()
            .map(|p| PatPlan {
                s: p.s,
                p: p.p,
                o: p.o,
            })
            .collect()
    }

    fn collect(f: impl FnOnce(&mut Sink<'_>) -> Flow) -> Vec<Binding> {
        let mut out = Vec::new();
        let _ = f(&mut |b| {
            out.push(b);
            GO
        });
        out
    }

    #[test]
    fn plans_stream_in_the_materialized_order_and_positions_agree() {
        let gp = bgp("?a :p0 ?b . ?b :p1 ?c . ?c :p2 ?d . ?d :p0 ?e");
        for seed in 0..6 {
            let (d, s) = graph(9, 40, seed);
            let ev = Evaluator::new(&d, &s);
            let layout = VarLayout::build(&gp);
            let (plans, cands) = compiled(&ev, &gp, &layout);
            let trees = [
                join(join(leaf(0), leaf(1)), join(leaf(2), leaf(3))),
                join(leaf(2), join(leaf(0), join(leaf(3), leaf(1)))),
                join(join(leaf(3), leaf(2)), join(leaf(1), leaf(0))),
            ];
            for tree in trees {
                let full = ev.eval_join_tree(&tree, &plans, &cands, &layout).0;
                let p = BgpPlan {
                    plans: copy_plans(&plans),
                    cands: cands.clone(),
                    exec: ExecPlan {
                        order: vec![0, 1, 2, 3],
                        tree: Some(tree.clone()),
                    },
                };
                let streamed = collect(|sink| ev.stream_join_tree(&tree, &p, &layout, sink));
                assert_eq!(streamed, full, "{tree:?}");
                let pos = ev.solution_positions(&p, &full, layout.len()).unwrap();
                assert!(pos.windows(2).all(|w| w[0] < w[1]), "{tree:?}");
            }
            for order in [[0, 1, 2, 3], [3, 2, 1, 0], [1, 3, 0, 2], [2, 0, 3, 1]] {
                let mut full = vec![vec![None; layout.len()]];
                for &pi in &order {
                    let mut next = Vec::new();
                    for b in &full {
                        ev.extend(b, &plans[pi], &cands, &mut next);
                    }
                    full = next;
                }
                let p = BgpPlan {
                    plans: copy_plans(&plans),
                    cands: cands.clone(),
                    exec: ExecPlan {
                        order: order.to_vec(),
                        tree: None,
                    },
                };
                let streamed =
                    collect(|sink| ev.stream_left_deep(&p, 0, vec![None; layout.len()], sink));
                assert_eq!(streamed, full, "{order:?}");
                let pos = ev.solution_positions(&p, &full, layout.len()).unwrap();
                assert!(pos.windows(2).all(|w| w[0] < w[1]), "{order:?}");
            }
        }
    }

    /// The streaming paths answer a `?s p ?o` scan from small candidate sets
    /// by lookups (as `extend` does): the same solutions, in the same order,
    /// as scanning the predicate and filtering each triple by the candidates.
    #[test]
    fn streaming_candidate_lookups_equal_the_filtered_scan() {
        let mut state = 0x7095_u64;
        let mut next = move |n: u32| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 33) % u64::from(n)) as u32
        };
        let mut store = TripleStore::new();
        store.bulk_load((0..6000).map(|_| IdTriple::new(next(400), next(3), next(500))));
        let dict = Dictionary::new();
        let ev = Evaluator::new(&dict, &store);
        let layout = VarLayout::build(&bgp("?a :p0 ?b"));
        assert_eq!(layout.len(), 2);
        for round in 0..45 {
            let p = next(3);
            let plan = PatPlan {
                s: Slot::Var(0),
                p: Slot::Const(p),
                o: Slot::Var(if round % 7 == 0 { 0 } else { 1 }),
            };
            let mut set = |n: u32, range: u32| -> Vec<u32> {
                let mut v: Vec<u32> = (0..n).map(|_| next(range)).collect();
                v.sort_unstable();
                v.dedup();
                v
            };
            let mut cands = Candidates::default();
            match round % 3 {
                0 => {
                    cands.insert(0, set(20, 400));
                }
                1 => {
                    cands.insert(1, set(15, 500));
                }
                _ => {
                    cands.insert(0, set(30, 400));
                    cands.insert(1, set(200, 500));
                }
            }
            let empty: Binding = vec![None; 2];
            let mut want = Vec::new();
            for (s, pr, o) in ev.candidate_triples(&empty, &plan) {
                if !cand_ok(&plan.s, s, &cands) || !cand_ok(&plan.o, o, &cands) {
                    continue;
                }
                let mut nb = empty.clone();
                if unify(&mut nb, &plan.s, s)
                    && unify(&mut nb, &plan.p, pr)
                    && unify(&mut nb, &plan.o, o)
                {
                    want.push(nb);
                }
            }
            let bp = BgpPlan {
                plans: copy_plans(std::slice::from_ref(&plan)),
                cands,
                exec: ExecPlan {
                    order: vec![0],
                    tree: None,
                },
            };
            let left_deep = collect(|sink| ev.stream_left_deep(&bp, 0, empty.clone(), sink));
            assert_eq!(left_deep, want, "left-deep, round {round}");
            let tree = collect(|sink| ev.stream_join_tree(&leaf(0), &bp, &layout, sink));
            assert_eq!(tree, want, "join-tree leaf, round {round}");
        }
    }

    /// The join order `eval_join` used before it was factored out.
    fn original_join_order(parts: &[(usize, Vec<usize>)]) -> Vec<usize> {
        let mut parts: Vec<(usize, Vec<usize>, usize)> = parts
            .iter()
            .enumerate()
            .map(|(i, (n, v))| (*n, v.clone(), i))
            .collect();
        let start = (0..parts.len()).min_by_key(|&i| parts[i].0).unwrap_or(0);
        let (_, mut acc_vars, first) = parts.swap_remove(start);
        let mut order = vec![first];
        while !parts.is_empty() {
            let pick = (0..parts.len())
                .min_by_key(|&i| {
                    let connected = parts[i].1.iter().any(|v| acc_vars.contains(v));
                    (!connected as u8, parts[i].0)
                })
                .unwrap();
            let (_, rv, idx) = parts.swap_remove(pick);
            for v in rv {
                if !acc_vars.contains(&v) {
                    acc_vars.push(v);
                }
            }
            order.push(idx);
        }
        order
    }

    #[test]
    fn greedy_join_order_is_the_original_order() {
        let mut rng = Rng(5);
        for _ in 0..2000 {
            let n = 1 + rng.below(7);
            let parts: Vec<(usize, Vec<usize>)> = (0..n)
                .map(|_| {
                    let vars = (0..rng.below(3)).map(|_| rng.below(6)).collect();
                    (rng.below(4), vars)
                })
                .collect();
            let meta: Vec<(usize, &[usize])> =
                parts.iter().map(|(n, v)| (*n, v.as_slice())).collect();
            assert_eq!(
                greedy_join_order(&meta),
                original_join_order(&parts),
                "{parts:?}"
            );
        }
    }

    #[test]
    fn probe_pairs_like_join_sets() {
        let mut rng = Rng(9);
        let width = 5;
        let rows = |vars: &[usize], rng: &mut Rng| -> Vec<Binding> {
            (0..rng.below(8))
                .map(|_| {
                    let mut b = vec![None; width];
                    for &v in vars {
                        if rng.below(5) != 0 {
                            b[v] = Some(rng.below(3) as u32);
                        }
                    }
                    b
                })
                .collect()
        };
        for _ in 0..500 {
            let lvars: Vec<usize> = (0..width).filter(|_| rng.below(2) == 0).collect();
            let rvars: Vec<usize> = (0..width).filter(|_| rng.below(2) == 0).collect();
            let left = rows(&lvars, &mut rng);
            let right = rows(&rvars, &mut rng);
            let shared: Vec<usize> = lvars
                .iter()
                .copied()
                .filter(|v| rvars.contains(v))
                .collect();
            let expected = join_sets(&left, &lvars, &right, &rvars);
            let probe = Probe::new(right.clone(), &shared);
            let mut got = Vec::new();
            for l in &left {
                let _ = probe.for_each_compatible(l, &mut |m| {
                    got.push(m);
                    GO
                });
            }
            assert_eq!(got, expected);
        }
    }

    #[test]
    fn linear_keys() {
        let order = |e: &str| {
            let q = parse(&format!(
                "SELECT * WHERE {{ ?x ?p ?y . ?y ?q ?z }} ORDER BY ({e})"
            ))
            .unwrap();
            let Query::Select(s) = q else { panic!() };
            let layout = VarLayout::build(&s.pattern);
            let key = linear_key(&s.order_by[0].expr, &layout);
            key.map(|k| {
                let coef: Vec<(String, i128)> = k
                    .coef
                    .iter()
                    .map(|(&v, &c)| (layout.names[v].clone(), c))
                    .collect();
                (coef, k.vars.len(), k.constant)
            })
        };
        let x = |c| ("x".to_string(), c);
        let y = |c| ("y".to_string(), c);
        let z = |c| ("z".to_string(), c);
        assert_eq!(order("?x"), Some((vec![x(1)], 1, 0)));
        assert_eq!(order("2*?x - 3*?y + 5"), Some((vec![x(2), y(-3)], 2, 5)));
        assert_eq!(order("(?x - ?y) * -2"), Some((vec![x(-2), y(2)], 2, 0)));
        assert_eq!(order("-(?x + ?z) + +?x"), Some((vec![x(0), z(-1)], 2, 0)));
        assert_eq!(order("0 * ?y"), Some((vec![y(0)], 1, 0)));
        assert_eq!(order("?x * ?y"), None);
        assert_eq!(order("?x + 0.5"), None);
        assert_eq!(order("?x / 2"), None);
        assert_eq!(order("ABS(?x)"), None);
        assert_eq!(order("?nope + 1"), None);
        assert_eq!(order("?x * 4294967297"), None, "coefficient bound");
    }
}
