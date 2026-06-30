//! BGP evaluation, joins, FILTER, and solution modifiers.
//!
//! Corresponds to gStore's `GeneralEvaluation` + `Database/Executor`/`Join`.
//! The pipeline:
//!
//! 1. Resolve each triple pattern's constants to ids (a missing constant makes
//!    the whole conjunctive BGP empty).
//! 2. Greedily order patterns (most-constrained / connected first) — a simple
//!    stand-in for gStore's cost-based `Optimizer` (backlog item C).
//! 3. Iteratively extend partial bindings by indexed lookups + unification.
//! 4. Apply FILTER, ORDER BY, DISTINCT, OFFSET/LIMIT, and projection.

use std::collections::HashMap;

use regex::Regex;

use crate::dict::Dictionary;
use crate::error::{GStoreError, Result};
use crate::model::id::PredId;
use crate::model::Term;
use crate::parser::ntriples::parse_term;
use crate::parser::sparql::ast::*;
use crate::store::TripleStore;

use super::results::{QueryResult, ResultSet};
use super::value::{order_key, Value};

/// A partial/complete solution: `var index → bound id` (None = unbound).
type Binding = Vec<Option<u32>>;

/// Variable bookkeeping shared across a query evaluation.
struct VarLayout {
    /// Variable name → dense index.
    index: HashMap<String, usize>,
    /// Index → name, in first-appearance order (drives `SELECT *`).
    names: Vec<String>,
    /// Whether each variable ever appears in predicate position (→ resolve its
    /// id via the predicate dictionary rather than the entity/literal one).
    is_pred: Vec<bool>,
}

impl VarLayout {
    fn build(pattern: &GraphPattern) -> VarLayout {
        let mut patterns = Vec::new();
        pattern.collect_triples(&mut patterns);
        let mut index = HashMap::new();
        let mut names = Vec::new();
        let mut is_pred = Vec::new();
        let add = |name: &str,
                   pred_pos: bool,
                   index: &mut HashMap<String, usize>,
                   names: &mut Vec<String>,
                   is_pred: &mut Vec<bool>| {
            let i = *index.entry(name.to_string()).or_insert_with(|| {
                names.push(name.to_string());
                is_pred.push(false);
                names.len() - 1
            });
            if pred_pos {
                is_pred[i] = true;
            }
        };
        for p in patterns {
            if let PatternTerm::Var(v) = &p.subject {
                add(v, false, &mut index, &mut names, &mut is_pred);
            }
            if let PatternTerm::Var(v) = &p.predicate {
                add(v, true, &mut index, &mut names, &mut is_pred);
            }
            if let PatternTerm::Var(v) = &p.object {
                add(v, false, &mut index, &mut names, &mut is_pred);
            }
        }
        VarLayout {
            index,
            names,
            is_pred,
        }
    }

    fn len(&self) -> usize {
        self.names.len()
    }
}

/// A pattern position resolved to either a constant id or a variable slot.
#[derive(Clone, Copy)]
enum Slot {
    Const(u32),
    Var(usize),
}

/// A triple pattern compiled to slots.
struct PatPlan {
    s: Slot,
    p: Slot,
    o: Slot,
}

/// The query evaluator binds a dictionary and a store for the duration of a query.
pub struct Evaluator<'a> {
    dict: &'a Dictionary,
    store: &'a TripleStore,
}

impl<'a> Evaluator<'a> {
    pub fn new(dict: &'a Dictionary, store: &'a TripleStore) -> Evaluator<'a> {
        Evaluator { dict, store }
    }

    /// Evaluate a read query (SELECT / ASK). Updates are handled in the db layer.
    pub fn evaluate(&self, query: &Query) -> Result<QueryResult> {
        match query {
            Query::Select(s) => Ok(QueryResult::Select(self.eval_select(s)?)),
            Query::Ask(a) => {
                let layout = VarLayout::build(&a.pattern);
                let solutions = self.eval_pattern(&a.pattern, &layout);
                Ok(QueryResult::Ask(!solutions.is_empty()))
            }
            Query::InsertData(_) | Query::DeleteData(_) => Err(GStoreError::Query(
                "updates must be applied through Database, not the read evaluator".into(),
            )),
        }
    }

    fn eval_select(&self, q: &SelectQuery) -> Result<ResultSet> {
        let layout = VarLayout::build(&q.pattern);
        // FILTERs live inside the pattern algebra and are applied during eval.
        let mut solutions = self.eval_pattern(&q.pattern, &layout);

        // ORDER BY (over full bindings, before projection/distinct).
        if !q.order_by.is_empty() {
            self.sort_solutions(&mut solutions, &q.order_by, &layout);
        }

        // Projection columns.
        let cols: Vec<usize> = match &q.projection {
            Projection::All => (0..layout.len()).collect(),
            Projection::Vars(vars) => vars
                .iter()
                .map(|v| {
                    layout.index.get(v).copied().ok_or_else(|| {
                        GStoreError::Query(format!("SELECT variable ?{v} not used in WHERE"))
                    })
                })
                .collect::<Result<Vec<_>>>()?,
        };
        let out_vars: Vec<String> = match &q.projection {
            Projection::All => layout.names.clone(),
            Projection::Vars(vars) => vars.clone(),
        };

        // Materialize rows (id → surface string).
        let mut rows: Vec<Vec<Option<String>>> = solutions
            .iter()
            .map(|b| {
                cols.iter()
                    .map(|&c| self.resolve_string(b[c], layout.is_pred[c]))
                    .collect()
            })
            .collect();

        // DISTINCT (stable: preserves the ORDER BY ordering above).
        if q.distinct {
            let mut seen = std::collections::HashSet::new();
            rows.retain(|r| seen.insert(r.clone()));
        }

        // OFFSET / LIMIT.
        let offset = q.offset.unwrap_or(0);
        if offset > 0 {
            rows.drain(0..offset.min(rows.len()));
        }
        if let Some(limit) = q.limit {
            rows.truncate(limit);
        }

        Ok(ResultSet {
            vars: out_vars,
            rows,
        })
    }

    // ---- graph-pattern algebra -------------------------------------------

    /// Evaluate a graph pattern to its full set of solution bindings.
    ///
    /// Each pattern is evaluated independently and combined: a `Bgp` runs as one
    /// index-pushed unit, a `Union` concatenates branches, and a `Join` is
    /// reordered (most selective first) and combined with hash joins — which
    /// avoids the cartesian blow-up of naive left-to-right evaluation.
    fn eval_pattern(&self, gp: &GraphPattern, layout: &VarLayout) -> Vec<Binding> {
        match gp {
            GraphPattern::Empty => vec![vec![None; layout.len()]],
            GraphPattern::Bgp(tps) => self.eval_bgp(tps, layout),
            GraphPattern::Union(a, b) => {
                let mut left = self.eval_pattern(a, layout);
                left.extend(self.eval_pattern(b, layout));
                left
            }
            GraphPattern::Filter(filters, inner) => {
                let mut r = self.eval_pattern(inner, layout);
                r.retain(|b| {
                    filters
                        .iter()
                        .all(|f| self.eval_ebv(f, b, layout) == Some(true))
                });
                r
            }
            GraphPattern::Join(_, _) => {
                let mut conjuncts = Vec::new();
                flatten_join(gp, &mut conjuncts);
                self.eval_join(&conjuncts, layout)
            }
        }
    }

    /// Evaluate a conjunction of patterns, joining smallest/connected first.
    fn eval_join(&self, conjuncts: &[&GraphPattern], layout: &VarLayout) -> Vec<Binding> {
        // Evaluate each conjunct independently, recording its variable set.
        let mut parts: Vec<(Vec<Binding>, Vec<usize>)> = conjuncts
            .iter()
            .map(|c| (self.eval_pattern(c, layout), pattern_vars(c, layout)))
            .collect();

        // Start from the smallest result, then greedily join in a connected,
        // smallest-first order (a lightweight join optimizer).
        let start = (0..parts.len())
            .min_by_key(|&i| parts[i].0.len())
            .unwrap_or(0);
        let (mut acc, mut acc_vars) = parts.swap_remove(start);

        while !parts.is_empty() {
            let pick = (0..parts.len())
                .min_by_key(|&i| {
                    let connected = parts[i].1.iter().any(|v| acc_vars.contains(v));
                    (!connected as u8, parts[i].0.len())
                })
                .unwrap();
            let (rb, rv) = parts.swap_remove(pick);
            acc = join_sets(&acc, &acc_vars, &rb, &rv);
            for v in rv {
                if !acc_vars.contains(&v) {
                    acc_vars.push(v);
                }
            }
            if acc.is_empty() {
                break;
            }
        }
        acc
    }

    /// Evaluate a basic graph pattern from scratch (index-pushed, ordered).
    fn eval_bgp(&self, tps: &[TriplePattern], layout: &VarLayout) -> Vec<Binding> {
        // Compile patterns; a missing constant ⇒ the conjunction is empty.
        let mut plans = Vec::with_capacity(tps.len());
        for tp in tps {
            match self.compile_pattern(tp, layout) {
                Some(plan) => plans.push(plan),
                None => return Vec::new(),
            }
        }
        if plans.is_empty() {
            return vec![vec![None; layout.len()]];
        }

        let order = self.order_plans(&plans);

        let mut solutions: Vec<Binding> = vec![vec![None; layout.len()]];
        for &pi in &order {
            let plan = &plans[pi];
            let mut next = Vec::new();
            for binding in &solutions {
                self.extend(binding, plan, &mut next);
            }
            solutions = next;
            if solutions.is_empty() {
                break;
            }
        }
        solutions
    }

    /// Resolve a pattern's constants to ids. Returns `None` if a constant is
    /// absent from the dictionary (no triple can match).
    fn compile_pattern(&self, tp: &TriplePattern, layout: &VarLayout) -> Option<PatPlan> {
        let s = self.compile_node_slot(&tp.subject, layout)?;
        let p = self.compile_pred_slot(&tp.predicate, layout)?;
        let o = self.compile_node_slot(&tp.object, layout)?;
        Some(PatPlan { s, p, o })
    }

    fn compile_node_slot(&self, pt: &PatternTerm, layout: &VarLayout) -> Option<Slot> {
        match pt {
            PatternTerm::Var(v) => Some(Slot::Var(layout.index[v])),
            PatternTerm::Term(t) => self.dict.term_id(t).map(Slot::Const),
        }
    }

    fn compile_pred_slot(&self, pt: &PatternTerm, layout: &VarLayout) -> Option<Slot> {
        match pt {
            PatternTerm::Var(v) => Some(Slot::Var(layout.index[v])),
            PatternTerm::Term(Term::Iri(iri)) => self
                .dict
                .predicate_id(&Term::iri(iri.clone()).dict_key())
                .map(Slot::Const),
            // A non-IRI predicate constant can never match.
            PatternTerm::Term(_) => None,
        }
    }

    /// Extend a partial binding by one pattern, appending results to `out`.
    fn extend(&self, binding: &Binding, plan: &PatPlan, out: &mut Vec<Binding>) {
        let ks = slot_known(&plan.s, binding);
        let kp = slot_known(&plan.p, binding);
        let ko = slot_known(&plan.o, binding);

        // Enumerate candidate (s, p, o) triples from the tightest index.
        let candidates: Vec<(u32, PredId, u32)> = match (ks, kp, ko) {
            (Some(s), Some(p), Some(o)) => {
                if self.store.exists(s, p, o) {
                    vec![(s, p, o)]
                } else {
                    vec![]
                }
            }
            (Some(s), Some(p), None) => self
                .store
                .o_by_sp(s, p)
                .into_iter()
                .map(|o| (s, p, o))
                .collect(),
            (Some(s), None, Some(o)) => self
                .store
                .p_by_so(s, o)
                .into_iter()
                .map(|p| (s, p, o))
                .collect(),
            (None, Some(p), Some(o)) => self
                .store
                .s_by_po(p, o)
                .into_iter()
                .map(|s| (s, p, o))
                .collect(),
            (Some(s), None, None) => self
                .store
                .po_by_s(s)
                .iter()
                .map(|&(p, o)| (s, p, o))
                .collect(),
            (None, Some(p), None) => self
                .store
                .so_by_p(p)
                .iter()
                .map(|&(s, o)| (s, p, o))
                .collect(),
            (None, None, Some(o)) => self
                .store
                .ps_by_o(o)
                .iter()
                .map(|&(p, s)| (s, p, o))
                .collect(),
            (None, None, None) => self
                .store
                .iter_all()
                .map(|t| (t.sub, t.pred, t.obj))
                .collect(),
        };

        for (s, p, o) in candidates {
            let mut nb = binding.clone();
            if unify(&mut nb, &plan.s, s)
                && unify(&mut nb, &plan.p, p)
                && unify(&mut nb, &plan.o, o)
            {
                out.push(nb);
            }
        }
    }

    // ---- plan ordering ----------------------------------------------------

    /// Greedy join order: prefer connected patterns, then more known positions,
    /// then smaller static cardinality.
    fn order_plans(&self, plans: &[PatPlan]) -> Vec<usize> {
        let n = plans.len();
        let mut remaining: Vec<usize> = (0..n).collect();
        let mut order = Vec::with_capacity(n);
        let mut bound: std::collections::HashSet<usize> = std::collections::HashSet::new();

        while !remaining.is_empty() {
            let first = order.is_empty();
            let best_pos = remaining
                .iter()
                .enumerate()
                .min_by_key(|&(_, &pi)| {
                    let plan = &plans[pi];
                    let connected = first || self.is_connected(plan, &bound);
                    let known = self.known_count(plan, &bound);
                    let card = self.static_card(plan);
                    // minimize: (not-connected, fewer-known, larger-card)
                    (!connected as u8, std::cmp::Reverse(known), card)
                })
                .map(|(pos, _)| pos)
                .unwrap();
            let pi = remaining.remove(best_pos);
            for slot in [&plans[pi].s, &plans[pi].p, &plans[pi].o] {
                if let Slot::Var(v) = slot {
                    bound.insert(*v);
                }
            }
            order.push(pi);
        }
        order
    }

    fn is_connected(&self, plan: &PatPlan, bound: &std::collections::HashSet<usize>) -> bool {
        [&plan.s, &plan.p, &plan.o].iter().any(|slot| match slot {
            Slot::Const(_) => true,
            Slot::Var(v) => bound.contains(v),
        })
    }

    fn known_count(&self, plan: &PatPlan, bound: &std::collections::HashSet<usize>) -> usize {
        [&plan.s, &plan.p, &plan.o]
            .iter()
            .filter(|slot| match slot {
                Slot::Const(_) => true,
                Slot::Var(v) => bound.contains(v),
            })
            .count()
    }

    /// A static cardinality estimate from constants only (exact where the index
    /// allows). Ignores runtime bindings — `known_count` already biases for those.
    fn static_card(&self, plan: &PatPlan) -> u64 {
        let s = as_const(&plan.s);
        let p = as_const(&plan.p);
        let o = as_const(&plan.o);
        match (s, p, o) {
            (Some(_), Some(_), Some(_)) => 1,
            (Some(s), Some(p), None) => self.store.o_by_sp(s, p).len() as u64,
            (None, Some(p), Some(o)) => self.store.s_by_po(p, o).len() as u64,
            (Some(s), None, Some(o)) => self.store.p_by_so(s, o).len() as u64,
            (Some(s), None, None) => self.store.po_by_s(s).len() as u64,
            (None, Some(p), None) => self.store.so_by_p(p).len() as u64,
            (None, None, Some(o)) => self.store.ps_by_o(o).len() as u64,
            (None, None, None) => self.store.triple_count(),
        }
    }

    // ---- value resolution -------------------------------------------------

    /// Resolve a bound id to its surface string for output.
    fn resolve_string(&self, id: Option<u32>, is_pred: bool) -> Option<String> {
        let id = id?;
        if is_pred {
            self.dict.predicate_to_string(id).map(str::to_owned)
        } else {
            self.dict.id_to_string(id).map(str::to_owned)
        }
    }

    /// Resolve a variable to a runtime [`Value`] for expression evaluation.
    fn resolve_value(&self, idx: usize, binding: &Binding, layout: &VarLayout) -> Option<Value> {
        let id = binding[idx]?;
        let s = if layout.is_pred[idx] {
            self.dict.predicate_to_string(id)?
        } else {
            self.dict.id_to_string(id)?
        };
        let term = parse_term(s).ok()?;
        Some(Value::from_term(&term))
    }

    // ---- expression evaluation -------------------------------------------

    /// Effective boolean value of an expression (error-tolerant for AND/OR).
    fn eval_ebv(&self, e: &Expr, b: &Binding, l: &VarLayout) -> Option<bool> {
        match e {
            Expr::And(x, y) => {
                let ex = self.eval_ebv(x, b, l);
                let ey = self.eval_ebv(y, b, l);
                if ex == Some(false) || ey == Some(false) {
                    Some(false)
                } else if ex == Some(true) && ey == Some(true) {
                    Some(true)
                } else {
                    None
                }
            }
            Expr::Or(x, y) => {
                let ex = self.eval_ebv(x, b, l);
                let ey = self.eval_ebv(y, b, l);
                if ex == Some(true) || ey == Some(true) {
                    Some(true)
                } else if ex == Some(false) && ey == Some(false) {
                    Some(false)
                } else {
                    None
                }
            }
            Expr::Not(x) => self.eval_ebv(x, b, l).map(|v| !v),
            _ => self.eval_value(e, b, l)?.ebv(),
        }
    }

    /// Evaluate an expression to a value (or `None` on error / unbound).
    fn eval_value(&self, e: &Expr, b: &Binding, l: &VarLayout) -> Option<Value> {
        match e {
            Expr::Var(name) => {
                let idx = *l.index.get(name)?;
                self.resolve_value(idx, b, l)
            }
            Expr::Const(t) => Some(Value::from_term(t)),
            Expr::Not(_) | Expr::And(_, _) | Expr::Or(_, _) => {
                self.eval_ebv(e, b, l).map(Value::Bool)
            }
            Expr::Compare(op, x, y) => self.eval_compare(*op, x, y, b, l).map(Value::Bool),
            Expr::Arith(op, x, y) => {
                let a = self.eval_value(x, b, l)?;
                let c = self.eval_value(y, b, l)?;
                eval_arith(*op, a, c)
            }
            Expr::Unary(op, x) => {
                let v = self.eval_value(x, b, l)?;
                match op {
                    UnaryOp::Plus => Some(v),
                    UnaryOp::Neg => match v {
                        Value::Int(i) => Some(Value::Int(-i)),
                        Value::Double(d) => Some(Value::Double(-d)),
                        _ => None,
                    },
                }
            }
            Expr::Builtin(name, args) => self.eval_builtin(name, args, b, l),
        }
    }

    fn eval_compare(
        &self,
        op: CompareOp,
        x: &Expr,
        y: &Expr,
        b: &Binding,
        l: &VarLayout,
    ) -> Option<bool> {
        let a = self.eval_value(x, b, l)?;
        let c = self.eval_value(y, b, l)?;
        match op {
            CompareOp::Eq => a.sparql_eq(&c),
            CompareOp::Ne => a.sparql_eq(&c).map(|v| !v),
            CompareOp::Lt => a.sparql_cmp(&c).map(|o| o.is_lt()),
            CompareOp::Gt => a.sparql_cmp(&c).map(|o| o.is_gt()),
            CompareOp::Le => a.sparql_cmp(&c).map(|o| o.is_le()),
            CompareOp::Ge => a.sparql_cmp(&c).map(|o| o.is_ge()),
        }
    }

    fn eval_builtin(&self, name: &str, args: &[Expr], b: &Binding, l: &VarLayout) -> Option<Value> {
        // BOUND inspects binding state directly (its arg may be unbound).
        if name == "BOUND" {
            if let [Expr::Var(v)] = args {
                let idx = *l.index.get(v)?;
                return Some(Value::Bool(b[idx].is_some()));
            }
            return None;
        }
        match name {
            "STR" => Some(Value::Str {
                value: self.eval_value(&args[0], b, l)?.lexical(),
                lang: None,
            }),
            "STRLEN" => {
                let v = self.eval_value(&args[0], b, l)?;
                Some(Value::Int(v.lexical().chars().count() as i64))
            }
            "UCASE" => self.str1(args, b, l, |s| s.to_uppercase()),
            "LCASE" => self.str1(args, b, l, |s| s.to_lowercase()),
            "ABS" => self.num1(args, b, l, f64::abs, i64::abs),
            "CEIL" => self.num1(args, b, l, f64::ceil, |i| i),
            "FLOOR" => self.num1(args, b, l, f64::floor, |i| i),
            "ROUND" => self.num1(args, b, l, f64::round, |i| i),
            "ISNUMERIC" => Some(Value::Bool(self.eval_value(&args[0], b, l)?.is_numeric())),
            "ISIRI" | "ISURI" => Some(Value::Bool(matches!(
                self.eval_value(&args[0], b, l)?,
                Value::Iri(_)
            ))),
            "ISBLANK" => Some(Value::Bool(matches!(
                self.eval_value(&args[0], b, l)?,
                Value::Blank(_)
            ))),
            "ISLITERAL" => Some(Value::Bool(matches!(
                self.eval_value(&args[0], b, l)?,
                Value::Int(_)
                    | Value::Double(_)
                    | Value::Bool(_)
                    | Value::Str { .. }
                    | Value::Typed { .. }
            ))),
            "LANG" => {
                let v = self.eval_value(&args[0], b, l)?;
                let lang = match v {
                    Value::Str { lang, .. } => lang.unwrap_or_default(),
                    _ => String::new(),
                };
                Some(Value::Str {
                    value: lang,
                    lang: None,
                })
            }
            "DATATYPE" => {
                let v = self.eval_value(&args[0], b, l)?;
                Some(Value::Iri(datatype_iri(&v)?))
            }
            "CONTAINS" => self.str2_bool(args, b, l, |a, c| a.contains(&c)),
            "STRSTARTS" => self.str2_bool(args, b, l, |a, c| a.starts_with(&c)),
            "STRENDS" => self.str2_bool(args, b, l, |a, c| a.ends_with(&c)),
            "STRBEFORE" => self.str2(args, b, l, |a, c| {
                a.split_once(&c)
                    .map(|(p, _)| p.to_string())
                    .unwrap_or_default()
            }),
            "STRAFTER" => self.str2(args, b, l, |a, c| {
                a.split_once(&c)
                    .map(|(_, s)| s.to_string())
                    .unwrap_or_default()
            }),
            "CONCAT" => {
                let mut out = String::new();
                for a in args {
                    out.push_str(&self.eval_value(a, b, l)?.lexical());
                }
                Some(Value::Str {
                    value: out,
                    lang: None,
                })
            }
            "SAMETERM" => {
                let a = self.eval_value(&args[0], b, l)?;
                let c = self.eval_value(&args[1], b, l)?;
                Some(Value::Bool(a == c))
            }
            "COALESCE" => args.iter().find_map(|a| self.eval_value(a, b, l)),
            "REGEX" => {
                let text = self.eval_value(&args[0], b, l)?.lexical();
                let pat = self.eval_value(&args[1], b, l)?.lexical();
                let flags = match args.get(2) {
                    Some(a) => self.eval_value(a, b, l)?.lexical(),
                    None => String::new(),
                };
                let pat = if flags.contains('i') {
                    format!("(?i){pat}")
                } else {
                    pat
                };
                let re = Regex::new(&pat).ok()?;
                Some(Value::Bool(re.is_match(&text)))
            }
            _ => None, // unimplemented builtin ⇒ error (excludes the solution)
        }
    }

    fn str1(
        &self,
        args: &[Expr],
        b: &Binding,
        l: &VarLayout,
        f: impl Fn(&str) -> String,
    ) -> Option<Value> {
        let v = self.eval_value(&args[0], b, l)?;
        Some(Value::Str {
            value: f(&v.lexical()),
            lang: None,
        })
    }

    fn str2(
        &self,
        args: &[Expr],
        b: &Binding,
        l: &VarLayout,
        f: impl Fn(String, String) -> String,
    ) -> Option<Value> {
        let a = self.eval_value(&args[0], b, l)?.lexical();
        let c = self.eval_value(&args[1], b, l)?.lexical();
        Some(Value::Str {
            value: f(a, c),
            lang: None,
        })
    }

    fn str2_bool(
        &self,
        args: &[Expr],
        b: &Binding,
        l: &VarLayout,
        f: impl Fn(String, String) -> bool,
    ) -> Option<Value> {
        let a = self.eval_value(&args[0], b, l)?.lexical();
        let c = self.eval_value(&args[1], b, l)?.lexical();
        Some(Value::Bool(f(a, c)))
    }

    fn num1(
        &self,
        args: &[Expr],
        b: &Binding,
        l: &VarLayout,
        ff: impl Fn(f64) -> f64,
        fi: impl Fn(i64) -> i64,
    ) -> Option<Value> {
        match self.eval_value(&args[0], b, l)? {
            Value::Int(i) => Some(Value::Int(fi(i))),
            Value::Double(d) => Some(Value::Double(ff(d))),
            _ => None,
        }
    }

    // ---- ordering ---------------------------------------------------------

    fn sort_solutions(
        &self,
        solutions: &mut [Binding],
        order_by: &[OrderCondition],
        layout: &VarLayout,
    ) {
        solutions.sort_by(|a, b| {
            for cond in order_by {
                let va = self.eval_value(&cond.expr, a, layout);
                let vb = self.eval_value(&cond.expr, b, layout);
                let ord = order_key(&va).cmp(&order_key(&vb));
                let ord = if cond.descending { ord.reverse() } else { ord };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });
    }
}

/// Flatten a (possibly nested) `Join` tree into its conjuncts.
fn flatten_join<'a>(gp: &'a GraphPattern, out: &mut Vec<&'a GraphPattern>) {
    match gp {
        GraphPattern::Join(a, b) => {
            flatten_join(a, out);
            flatten_join(b, out);
        }
        other => out.push(other),
    }
}

/// The variable indices a pattern structurally binds.
fn pattern_vars(gp: &GraphPattern, layout: &VarLayout) -> Vec<usize> {
    let mut tps = Vec::new();
    gp.collect_triples(&mut tps);
    let mut vars = Vec::new();
    for tp in tps {
        for pos in [&tp.subject, &tp.predicate, &tp.object] {
            if let Some(v) = pos.as_var() {
                let idx = layout.index[v];
                if !vars.contains(&idx) {
                    vars.push(idx);
                }
            }
        }
    }
    vars
}

/// Merge two compatible bindings; `None` if they conflict on a shared variable.
fn merge_bindings(l: &Binding, r: &Binding) -> Option<Binding> {
    let mut out = l.clone();
    for (i, &rv) in r.iter().enumerate() {
        if let Some(rv) = rv {
            match out[i] {
                Some(lv) if lv != rv => return None,
                _ => out[i] = Some(rv),
            }
        }
    }
    Some(out)
}

/// Join two solution sets on their shared variables (SPARQL compatible-merge).
/// Uses a hash join keyed on shared variables when they exist, else a product.
fn join_sets(
    left: &[Binding],
    left_vars: &[usize],
    right: &[Binding],
    right_vars: &[usize],
) -> Vec<Binding> {
    let shared: Vec<usize> = left_vars
        .iter()
        .copied()
        .filter(|v| right_vars.contains(v))
        .collect();

    let mut out = Vec::new();
    if shared.is_empty() {
        // No shared variable ⇒ cross product (disconnected pattern).
        for l in left {
            for r in right {
                if let Some(m) = merge_bindings(l, r) {
                    out.push(m);
                }
            }
        }
        return out;
    }

    // Hash the (usually smaller) right side on the shared-variable key.
    let key_of = |b: &Binding| -> Vec<Option<u32>> { shared.iter().map(|&i| b[i]).collect() };
    let mut index: HashMap<Vec<Option<u32>>, Vec<&Binding>> = HashMap::new();
    for r in right {
        index.entry(key_of(r)).or_default().push(r);
    }
    for l in left {
        if let Some(matches) = index.get(&key_of(l)) {
            for r in matches {
                if let Some(m) = merge_bindings(l, r) {
                    out.push(m);
                }
            }
        }
    }
    out
}

/// The known id at a slot given the current binding (const, or bound var).
fn slot_known(slot: &Slot, binding: &Binding) -> Option<u32> {
    match slot {
        Slot::Const(id) => Some(*id),
        Slot::Var(v) => binding[*v],
    }
}

fn as_const(slot: &Slot) -> Option<u32> {
    match slot {
        Slot::Const(id) => Some(*id),
        Slot::Var(_) => None,
    }
}

/// Unify a slot against a concrete id, mutating the binding. Returns false on
/// conflict (a variable already bound to a different id).
fn unify(binding: &mut Binding, slot: &Slot, value: u32) -> bool {
    match slot {
        Slot::Const(id) => *id == value,
        Slot::Var(v) => match binding[*v] {
            Some(existing) => existing == value,
            None => {
                binding[*v] = Some(value);
                true
            }
        },
    }
}

/// Numeric arithmetic with integer fast-path and f64 fallback.
fn eval_arith(op: ArithOp, a: Value, b: Value) -> Option<Value> {
    if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
        let (x, y) = (*x, *y);
        return Some(match op {
            ArithOp::Add => x
                .checked_add(y)
                .map(Value::Int)
                .unwrap_or(Value::Double(x as f64 + y as f64)),
            ArithOp::Sub => x
                .checked_sub(y)
                .map(Value::Int)
                .unwrap_or(Value::Double(x as f64 - y as f64)),
            ArithOp::Mul => x
                .checked_mul(y)
                .map(Value::Int)
                .unwrap_or(Value::Double(x as f64 * y as f64)),
            ArithOp::Div => {
                if y == 0 {
                    return None;
                }
                // SPARQL integer division yields a decimal.
                Value::Double(x as f64 / y as f64)
            }
        });
    }
    let x = a.as_f64()?;
    let y = b.as_f64()?;
    Some(Value::Double(match op {
        ArithOp::Add => x + y,
        ArithOp::Sub => x - y,
        ArithOp::Mul => x * y,
        ArithOp::Div => {
            if y == 0.0 {
                return None;
            }
            x / y
        }
    }))
}

/// The datatype IRI for `DATATYPE()`.
fn datatype_iri(v: &Value) -> Option<String> {
    Some(
        match v {
            Value::Int(_) => xsd::INTEGER,
            Value::Double(_) => xsd::DOUBLE,
            Value::Bool(_) => xsd::BOOLEAN,
            Value::Str { lang: None, .. } => xsd::STRING,
            Value::Str { lang: Some(_), .. } => {
                return Some("http://www.w3.org/1999/02/22-rdf-syntax-ns#langString".into())
            }
            Value::Typed { datatype, .. } => return Some(datatype.clone()),
            // IRIs and blank nodes have no datatype.
            Value::Iri(_) | Value::Blank(_) => return None,
        }
        .to_string(),
    )
}
