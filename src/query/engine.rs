//! BGP evaluation, joins, FILTER, and solution modifiers.
//!
//! Corresponds to gStore's `GeneralEvaluation` + `Database/Executor`/`Join`.
//! The pipeline:
//!
//! 1. Resolve each triple pattern's constants to ids (a missing constant makes
//!    the whole conjunctive BGP empty).
//! 2. (Optional) use the VS-tree to pre-compute candidate id sets for entity
//!    variables — a sound superset filter pushed into the join.
//! 3. Cost-based join ordering (see [`crate::query::optimizer`]).
//! 4. Iteratively extend partial bindings by indexed lookups + unification.
//! 5. Apply FILTER, ORDER BY, DISTINCT, OFFSET/LIMIT, and projection.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use regex::Regex;

use crate::dict::Dictionary;
use crate::error::{GStoreError, Result};
use crate::model::id::PredId;
use crate::model::{Term, Triple};
use crate::parser::ntriples::parse_term;
use crate::parser::sparql::ast::*;
use crate::signature::{EdgeDir, Signature, VsTree};
use crate::store::{TripleSource, TripleStore};

use super::candidates::{self, Candidates};
use super::optimizer::{self, ExecPlan, JoinTree};
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
        // All variables the pattern can bind (BGP, BIND, VALUES, sub-SELECT, …).
        let mut names = Vec::new();
        pattern.collect_vars(&mut names);
        let index: HashMap<String, usize> = names
            .iter()
            .enumerate()
            .map(|(i, v)| (v.clone(), i))
            .collect();
        let mut is_pred = vec![false; names.len()];
        // Mark variables that appear in predicate position of any triple.
        let mut tps = Vec::new();
        pattern.collect_triples(&mut tps);
        for tp in tps {
            if let PatternTerm::Var(v) = &tp.predicate {
                is_pred[index[v]] = true;
            }
        }
        VarLayout {
            index,
            names,
            is_pred,
        }
    }

    /// Build a layout directly from a known variable list (for result rows).
    fn from_vars(names: Vec<String>, is_pred: Vec<bool>) -> VarLayout {
        let index = names
            .iter()
            .enumerate()
            .map(|(i, v)| (v.clone(), i))
            .collect();
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
pub(crate) enum Slot {
    Const(u32),
    Var(usize),
}

impl Slot {
    /// The variable index, if this slot is a variable.
    pub(crate) fn var(&self) -> Option<usize> {
        match self {
            Slot::Var(v) => Some(*v),
            Slot::Const(_) => None,
        }
    }
}

/// A triple pattern compiled to slots.
pub(crate) struct PatPlan {
    pub(crate) s: Slot,
    pub(crate) p: Slot,
    pub(crate) o: Slot,
}

/// Synthetic-id base for *computed* terms (BIND/VALUES/aggregate results) that
/// are not in the dictionary. Bindings stay `u32` ids — keeping the join engine
/// fast — and computed terms are interned here, above the entity/literal ranges.
const SYNTH_BASE: u32 = 4_200_000_000;

/// Is `id` a synthetic (computed) term id rather than a dictionary id?
fn is_synth(id: u32) -> bool {
    id >= SYNTH_BASE
}

/// Per-query interner for computed terms, mapping them to synthetic ids so they
/// flow through the id-based join/binding machinery like any other term.
#[derive(Default)]
struct Extras {
    terms: Vec<Term>,
    index: HashMap<Term, u32>,
}

impl Extras {
    fn intern(&mut self, t: &Term) -> u32 {
        if let Some(&id) = self.index.get(t) {
            return id;
        }
        let id = SYNTH_BASE + self.terms.len() as u32;
        self.terms.push(t.clone());
        self.index.insert(t.clone(), id);
        id
    }
    fn term(&self, id: u32) -> Option<&Term> {
        self.terms.get((id - SYNTH_BASE) as usize)
    }
}

/// The query evaluator binds a dictionary and a store for the duration of a
/// query. It is generic over the [`TripleSource`] so the same optimizer and
/// executor run against the in-memory [`TripleStore`] or the on-disk store,
/// streaming index ranges from disk in the latter case.
pub struct Evaluator<'a, S: TripleSource = TripleStore> {
    dict: &'a Dictionary,
    store: &'a S,
    /// Optional VS-tree for entity-candidate pre-filtering.
    vstree: Option<&'a VsTree>,
    /// Named graphs (graph-IRI entity id → its store), for `GRAPH` patterns.
    named: Option<&'a BTreeMap<u32, TripleStore>>,
    /// Interner for computed terms produced during evaluation. Shared (`Rc`) with
    /// `GRAPH` sub-evaluators so synthetic ids minted inside a `GRAPH { … }` block
    /// resolve back in the outer evaluator.
    extras: Rc<std::cell::RefCell<Extras>>,
    /// Cost-based plan cache (gStore `plan_cache`): structurally-identical BGPs —
    /// e.g. a BGP repeated across sub-SELECTs — reuse the DP-optimized plan
    /// instead of re-running plan enumeration. Keyed by the compiled patterns.
    plan_cache: std::cell::RefCell<HashMap<u64, ExecPlan>>,
}

impl<'a, S: TripleSource> Evaluator<'a, S> {
    pub fn new(dict: &'a Dictionary, store: &'a S) -> Evaluator<'a, S> {
        Evaluator {
            dict,
            store,
            vstree: None,
            named: None,
            extras: Rc::new(std::cell::RefCell::new(Extras::default())),
            plan_cache: std::cell::RefCell::new(HashMap::new()),
        }
    }

    /// Build an evaluator that uses `vstree` to prune entity-variable bindings.
    pub fn with_vstree(
        dict: &'a Dictionary,
        store: &'a S,
        vstree: &'a VsTree,
    ) -> Evaluator<'a, S> {
        Evaluator {
            dict,
            store,
            vstree: Some(vstree),
            named: None,
            extras: Rc::new(std::cell::RefCell::new(Extras::default())),
            plan_cache: std::cell::RefCell::new(HashMap::new()),
        }
    }

    /// Attach named graphs so `GRAPH` patterns can be evaluated.
    pub fn with_named(mut self, named: &'a BTreeMap<u32, TripleStore>) -> Evaluator<'a, S> {
        self.named = Some(named);
        self
    }

    /// Convert a term to an id: its dictionary id if present, else a synthetic
    /// id (so computed/unknown terms still join and render consistently).
    fn term_to_id(&self, t: &Term) -> u32 {
        self.dict
            .term_id(t)
            .unwrap_or_else(|| self.extras.borrow_mut().intern(t))
    }

    /// Intern a runtime [`Value`] as an id (via its term form).
    fn value_to_id(&self, v: &Value) -> u32 {
        self.term_to_id(&value_to_term(v))
    }

    /// Evaluate a read query (SELECT / ASK / CONSTRUCT). Updates are handled in
    /// the db layer.
    pub fn evaluate(&self, query: &Query) -> Result<QueryResult> {
        match query {
            Query::Select(s) => Ok(QueryResult::Select(self.eval_select(s)?)),
            Query::Ask(a) => {
                let layout = VarLayout::build(&a.pattern);
                let solutions = self.eval_pattern(&a.pattern, &layout);
                Ok(QueryResult::Ask(!solutions.is_empty()))
            }
            Query::Construct(c) => Ok(QueryResult::Construct(self.eval_construct(c)?)),
            Query::Describe(d) => Ok(QueryResult::Construct(self.eval_describe(d)?)),
            Query::Update(_) => Err(GStoreError::Query(
                "updates must be applied through Database, not the read evaluator".into(),
            )),
        }
    }

    /// Evaluate CONSTRUCT: instantiate the template once per solution, skipping
    /// any template triple with an unbound variable. Returns de-duplicated triples.
    fn eval_construct(&self, c: &ConstructQuery) -> Result<Vec<Triple>> {
        let layout = VarLayout::build(&c.pattern);
        let mut solutions = self.eval_pattern(&c.pattern, &layout);
        if !c.order_by.is_empty() {
            self.sort_solutions(&mut solutions, &c.order_by, &layout);
        }
        let offset = c.offset.unwrap_or(0);
        if offset > 0 {
            solutions.drain(0..offset.min(solutions.len()));
        }
        if let Some(limit) = c.limit {
            solutions.truncate(limit);
        }

        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for b in &solutions {
            for tp in &c.template {
                if let Some(t) = self.instantiate_template(tp, b, &layout) {
                    if seen.insert(t.to_string()) {
                        out.push(t);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Evaluate DESCRIBE: collect the resource ids (explicit terms + variable
    /// targets bound by the WHERE pattern) and return every triple having one of
    /// them as subject (the outgoing description), de-duplicated.
    fn eval_describe(&self, d: &DescribeQuery) -> Result<Vec<Triple>> {
        let mut ids: Vec<u32> = Vec::new();
        let add = |id: u32, ids: &mut Vec<u32>| {
            if !ids.contains(&id) {
                ids.push(id);
            }
        };

        // Explicit term targets resolve directly through the dictionary.
        for t in &d.targets {
            if let PatternTerm::Term(term) = t {
                if let Some(id) = self.dict.term_id(term) {
                    add(id, &mut ids);
                }
            }
        }

        // `DESCRIBE *` or variable targets need the WHERE solutions.
        let needs_vars = d.all || d.targets.iter().any(|t| matches!(t, PatternTerm::Var(_)));
        if needs_vars {
            if let Some(pat) = &d.pattern {
                let layout = VarLayout::build(pat);
                let sols = self.eval_pattern(pat, &layout);
                // Only describe resource-valued columns: predicate-typed columns
                // hold predicate ids (a different id space), and synthetic ids are
                // computed values, not stored resources.
                let want: Vec<usize> = if d.all {
                    (0..layout.len()).filter(|&i| !layout.is_pred[i]).collect()
                } else {
                    d.targets
                        .iter()
                        .filter_map(|t| match t {
                            PatternTerm::Var(v) => layout.index.get(v).copied(),
                            _ => None,
                        })
                        .filter(|&i| !layout.is_pred[i])
                        .collect()
                };
                for s in &sols {
                    for &i in &want {
                        if let Some(id) = s[i] {
                            if !is_synth(id) {
                                add(id, &mut ids);
                            }
                        }
                    }
                }
            }
        }

        // Gather outgoing triples for each resource.
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for id in ids {
            let Some(subj) = self
                .resolve_string(Some(id), false)
                .and_then(|s| parse_term(&s).ok())
            else {
                continue;
            };
            for (p, o) in self.store.po_by_s(id) {
                let (Some(pred), Some(obj)) = (
                    self.resolve_string(Some(p), true)
                        .and_then(|s| parse_term(&s).ok()),
                    self.resolve_string(Some(o), false)
                        .and_then(|s| parse_term(&s).ok()),
                ) else {
                    continue;
                };
                let triple = Triple::new(subj.clone(), pred, obj);
                if seen.insert(triple.to_string()) {
                    out.push(triple);
                }
            }
        }
        Ok(out)
    }

    /// Compute the ground triples a `DELETE … INSERT … WHERE` modify would
    /// remove and add: evaluate the WHERE pattern, then instantiate each
    /// template against every solution (skipping template triples with an
    /// unbound variable), de-duplicated. The caller (Database) applies deletes
    /// before inserts. Read-only — does not mutate the store.
    pub fn eval_update_modify(
        &self,
        delete: &[TriplePattern],
        insert: &[TriplePattern],
        pattern: &GraphPattern,
    ) -> (Vec<Triple>, Vec<Triple>) {
        let layout = VarLayout::build(pattern);
        let sols = self.eval_pattern(pattern, &layout);
        let mut dels = Vec::new();
        let mut ins = Vec::new();
        let mut seen_d = HashSet::new();
        let mut seen_i = HashSet::new();
        for b in &sols {
            for tp in delete {
                if let Some(t) = self.instantiate_template(tp, b, &layout) {
                    if seen_d.insert(t.to_string()) {
                        dels.push(t);
                    }
                }
            }
            for tp in insert {
                if let Some(t) = self.instantiate_template(tp, b, &layout) {
                    if seen_i.insert(t.to_string()) {
                        ins.push(t);
                    }
                }
            }
        }
        (dels, ins)
    }

    /// Fill a template triple from a solution; `None` if any position is unbound.
    fn instantiate_template(
        &self,
        tp: &TriplePattern,
        b: &Binding,
        layout: &VarLayout,
    ) -> Option<Triple> {
        let resolve = |pt: &PatternTerm, is_pred: bool| -> Option<Term> {
            match pt {
                PatternTerm::Term(t) => Some(t.clone()),
                PatternTerm::Var(v) => {
                    let idx = *layout.index.get(v)?;
                    let s = self.resolve_string(b[idx], is_pred && layout.is_pred[idx])?;
                    parse_term(&s).ok()
                }
            }
        };
        Some(Triple::new(
            resolve(&tp.subject, false)?,
            resolve(&tp.predicate, true)?,
            resolve(&tp.object, false)?,
        ))
    }

    fn eval_select(&self, q: &SelectQuery) -> Result<ResultSet> {
        let (vars, is_pred, rows) = self.eval_select_solutions(q)?;
        let str_rows: Vec<Vec<Option<String>>> = rows
            .iter()
            .map(|r| {
                r.iter()
                    .enumerate()
                    .map(|(i, &id)| self.resolve_string(id, is_pred[i]))
                    .collect()
            })
            .collect();
        Ok(ResultSet {
            vars,
            rows: str_rows,
        })
    }

    /// Evaluate a SELECT into id-space result rows over its result variables.
    /// Returns `(result vars, per-column is_pred, rows)`. Used both for the
    /// top-level result and for sub-`SELECT`s (shared dictionary + extras).
    #[allow(clippy::type_complexity)]
    fn eval_select_solutions(
        &self,
        q: &SelectQuery,
    ) -> Result<(Vec<String>, Vec<bool>, Vec<Vec<Option<u32>>>)> {
        let layout = VarLayout::build(&q.pattern);
        let solutions = self.eval_pattern(&q.pattern, &layout);

        let result_vars = q.result_vars();
        // A result column is predicate-typed only if it is a plain pattern
        // variable that occurs in predicate position.
        let is_pred: Vec<bool> = result_vars
            .iter()
            .map(|v| layout.index.get(v).is_some_and(|&i| layout.is_pred[i]))
            .collect();

        let has_agg = !q.group_by.is_empty() || projection_has_aggregate(&q.projection);
        let mut rows = if has_agg {
            self.eval_aggregation(&solutions, &layout, q)?
        } else {
            solutions
                .iter()
                .map(|b| self.project_items(b, &layout, q))
                .collect::<Result<Vec<_>>>()?
        };

        // ORDER BY / DISTINCT / OFFSET / LIMIT over the result rows.
        let result_layout = VarLayout::from_vars(result_vars.clone(), is_pred.clone());
        if !q.order_by.is_empty() {
            self.sort_solutions(&mut rows, &q.order_by, &result_layout);
        }
        if q.distinct {
            let mut seen = std::collections::HashSet::new();
            rows.retain(|r| seen.insert(r.clone()));
        }
        let offset = q.offset.unwrap_or(0);
        if offset > 0 {
            rows.drain(0..offset.min(rows.len()));
        }
        if let Some(limit) = q.limit {
            rows.truncate(limit);
        }

        Ok((result_vars, is_pred, rows))
    }

    /// Project a single solution into result-variable order (no aggregation).
    fn project_items(
        &self,
        b: &Binding,
        layout: &VarLayout,
        q: &SelectQuery,
    ) -> Result<Vec<Option<u32>>> {
        match &q.projection {
            Projection::All => Ok(layout.names.iter().map(|v| b[layout.index[v]]).collect()),
            Projection::Items(items) => items
                .iter()
                .map(|it| match it {
                    SelectItem::Var(v) => Ok(layout.index.get(v).and_then(|&i| b[i])),
                    SelectItem::Expr(e, _) => {
                        Ok(self.eval_value(e, b, layout).map(|v| self.value_to_id(&v)))
                    }
                })
                .collect(),
        }
    }

    /// Group solutions and compute aggregates / projected expressions.
    fn eval_aggregation(
        &self,
        solutions: &[Binding],
        layout: &VarLayout,
        q: &SelectQuery,
    ) -> Result<Vec<Vec<Option<u32>>>> {
        // Group key = the GROUP BY expression values (empty ⇒ one group).
        use std::collections::hash_map::Entry;
        let mut order: Vec<Vec<Option<u32>>> = Vec::new();
        let mut groups: HashMap<Vec<Option<u32>>, Vec<usize>> = HashMap::new();
        for (i, b) in solutions.iter().enumerate() {
            let key: Vec<Option<u32>> = q
                .group_by
                .iter()
                .map(|e| self.eval_value(e, b, layout).map(|v| self.value_to_id(&v)))
                .collect();
            match groups.entry(key.clone()) {
                Entry::Occupied(mut o) => o.get_mut().push(i),
                Entry::Vacant(v) => {
                    order.push(key);
                    v.insert(vec![i]);
                }
            }
        }
        // Aggregating with no GROUP BY over zero rows still yields one group.
        if q.group_by.is_empty() && solutions.is_empty() {
            order.push(Vec::new());
            groups.insert(Vec::new(), Vec::new());
        }

        // Map GROUP BY variables to their key column, for projecting group vars.
        let mut key_vars: HashMap<String, usize> = HashMap::new();
        for (ki, e) in q.group_by.iter().enumerate() {
            if let Expr::Var(v) = e {
                key_vars.insert(v.clone(), ki);
            }
        }

        let items = match &q.projection {
            Projection::Items(items) => items,
            Projection::All => {
                return Err(GStoreError::Query(
                    "SELECT * with aggregation is not allowed".into(),
                ))
            }
        };

        let mut rows = Vec::new();
        for key in &order {
            let sols: Vec<&Binding> = groups[key].iter().map(|&i| &solutions[i]).collect();
            // HAVING: drop groups failing any constraint.
            let keep = q.having.iter().all(|h| {
                self.eval_group_expr(h, &sols, layout, &key_vars, key)
                    .and_then(|v| v.ebv())
                    == Some(true)
            });
            if !keep {
                continue;
            }
            let mut row = Vec::with_capacity(items.len());
            for it in items {
                let val = match it {
                    SelectItem::Var(v) => key_vars.get(v).and_then(|&ki| key[ki]).or_else(|| {
                        sols.first()
                            .and_then(|b| layout.index.get(v).and_then(|&i| b[i]))
                    }),
                    SelectItem::Expr(e, _) => self
                        .eval_group_expr(e, &sols, layout, &key_vars, key)
                        .map(|v| self.value_to_id(&v)),
                };
                row.push(val);
            }
            rows.push(row);
        }
        Ok(rows)
    }

    // ---- aggregate / group-expression evaluation -------------------------

    /// Evaluate an expression in group context (may contain aggregates).
    fn eval_group_expr(
        &self,
        e: &Expr,
        sols: &[&Binding],
        layout: &VarLayout,
        key_vars: &HashMap<String, usize>,
        key: &[Option<u32>],
    ) -> Option<Value> {
        match e {
            Expr::Aggregate {
                func,
                distinct,
                arg,
                sep,
            } => self.eval_aggregate(
                *func,
                *distinct,
                arg.as_deref(),
                sep.as_deref(),
                sols,
                layout,
            ),
            Expr::Var(name) => {
                if let Some(&ki) = key_vars.get(name) {
                    self.id_to_value(key[ki]?, false)
                } else {
                    // Non-grouped variable inside an expression ⇒ SAMPLE-like.
                    let idx = *layout.index.get(name)?;
                    self.resolve_value(idx, sols.first()?, layout)
                }
            }
            Expr::Const(t) => Some(Value::from_term(t)),
            Expr::Arith(op, a, b) => {
                let x = self.eval_group_expr(a, sols, layout, key_vars, key)?;
                let y = self.eval_group_expr(b, sols, layout, key_vars, key)?;
                eval_arith(*op, x, y)
            }
            Expr::Unary(op, x) => {
                let v = self.eval_group_expr(x, sols, layout, key_vars, key)?;
                match op {
                    UnaryOp::Plus => Some(v),
                    UnaryOp::Neg => match v {
                        Value::Int(i) => Some(
                            i.checked_neg()
                                .map(Value::Int)
                                .unwrap_or_else(|| Value::Double(-(i as f64))),
                        ),
                        Value::Double(d) => Some(Value::Double(-d)),
                        _ => None,
                    },
                }
            }
            Expr::Compare(op, a, b) => {
                let x = self.eval_group_expr(a, sols, layout, key_vars, key)?;
                let y = self.eval_group_expr(b, sols, layout, key_vars, key)?;
                Some(Value::Bool(compare_values(*op, &x, &y)?))
            }
            Expr::And(a, b) => {
                let x = self.eval_group_expr(a, sols, layout, key_vars, key)?.ebv();
                let y = self.eval_group_expr(b, sols, layout, key_vars, key)?.ebv();
                Some(Value::Bool(x? && y?))
            }
            Expr::Or(a, b) => {
                let x = self.eval_group_expr(a, sols, layout, key_vars, key)?.ebv();
                let y = self.eval_group_expr(b, sols, layout, key_vars, key)?.ebv();
                Some(Value::Bool(x? || y?))
            }
            Expr::Not(x) => Some(Value::Bool(
                !self
                    .eval_group_expr(x, sols, layout, key_vars, key)?
                    .ebv()?,
            )),
            // Builtins over aggregate context are not supported.
            Expr::Builtin(_, _) => None,
            // EXISTS in HAVING: test against the group's representative solution.
            Expr::Exists(neg, pat) => {
                let rep = *sols.first()?;
                Some(Value::Bool(self.eval_exists(*neg, pat, rep, layout)))
            }
        }
    }

    /// Compute one aggregate over a group's solutions.
    fn eval_aggregate(
        &self,
        func: AggFunc,
        distinct: bool,
        arg: Option<&Expr>,
        sep: Option<&str>,
        sols: &[&Binding],
        layout: &VarLayout,
    ) -> Option<Value> {
        // COUNT(*) counts solutions.
        if func == AggFunc::Count && arg.is_none() {
            return Some(Value::Int(sols.len() as i64));
        }
        let arg = arg?;
        let mut vals: Vec<Value> = sols
            .iter()
            .filter_map(|b| self.eval_value(arg, b, layout))
            .collect();
        if distinct {
            let mut seen: Vec<Value> = Vec::new();
            vals.retain(|v| {
                if seen.contains(v) {
                    false
                } else {
                    seen.push(v.clone());
                    true
                }
            });
        }
        Some(match func {
            AggFunc::Count => Value::Int(vals.len() as i64),
            AggFunc::Sum => sum_values(&vals),
            AggFunc::Avg => {
                if vals.is_empty() {
                    Value::Int(0)
                } else {
                    let s = sum_values(&vals).as_f64().unwrap_or(0.0);
                    Value::Double(s / vals.len() as f64)
                }
            }
            AggFunc::Min => vals.into_iter().reduce(|a, b| {
                if b.sparql_cmp(&a) == Some(std::cmp::Ordering::Less) {
                    b
                } else {
                    a
                }
            })?,
            AggFunc::Max => vals.into_iter().reduce(|a, b| {
                if b.sparql_cmp(&a) == Some(std::cmp::Ordering::Greater) {
                    b
                } else {
                    a
                }
            })?,
            AggFunc::Sample => vals.into_iter().next()?,
            AggFunc::GroupConcat => {
                let s = sep.unwrap_or(" ");
                let joined = vals.iter().map(Value::lexical).collect::<Vec<_>>().join(s);
                Value::Str {
                    value: joined,
                    lang: None,
                }
            }
        })
    }

    // ---- property paths --------------------------------------------------

    /// Evaluate a single property-path pattern into bindings over the layout.
    fn eval_path_pattern(&self, p: &PathPattern, layout: &VarLayout) -> Vec<Binding> {
        let subj_const = match &p.subject {
            PatternTerm::Term(t) => Some(self.dict.term_id(t)),
            PatternTerm::Var(_) => None,
        };
        let obj_const = match &p.object {
            PatternTerm::Term(t) => Some(self.dict.term_id(t)),
            PatternTerm::Var(_) => None,
        };
        // A constant endpoint absent from the dictionary ⇒ no matches.
        if matches!(subj_const, Some(None)) || matches!(obj_const, Some(None)) {
            return Vec::new();
        }
        let subj_id = subj_const.flatten();
        let obj_id = obj_const.flatten();

        let pairs = self.path_pairs(&p.path, subj_id, obj_id);

        pairs
            .into_iter()
            .filter_map(|(s, o)| {
                let mut b = vec![None; layout.len()];
                if !bind_node(&mut b, &p.subject, s, layout)
                    || !bind_node(&mut b, &p.object, o, layout)
                {
                    return None;
                }
                Some(b)
            })
            .collect()
    }

    /// All `(start, end)` pairs matching `path`, constrained by known endpoints.
    fn path_pairs(&self, path: &PathExpr, subj: Option<u32>, obj: Option<u32>) -> Vec<(u32, u32)> {
        match (subj, obj) {
            (Some(s), _) => {
                let ends = self.path_reach(path, s, false);
                ends.into_iter()
                    .filter(|e| obj.is_none_or(|o| o == *e))
                    .map(|e| (s, e))
                    .collect()
            }
            (None, Some(o)) => {
                // Walk the path backwards from the object.
                let starts = self.path_reach(path, o, true);
                starts.into_iter().map(|s| (s, o)).collect()
            }
            (None, None) => {
                // Both ends free: enumerate from every entity node.
                let mut nodes: Vec<u32> = self.store.subject_keys();
                nodes.extend(self.store.object_keys());
                nodes.sort_unstable();
                nodes.dedup();
                let mut out = Vec::new();
                for s in nodes {
                    for e in self.path_reach(path, s, false) {
                        out.push((s, e));
                    }
                }
                out
            }
        }
    }

    /// Nodes reachable from `start` by following `path` (or, if `reverse`, the
    /// nodes from which `start` is reachable via `path`).
    fn path_reach(
        &self,
        path: &PathExpr,
        start: u32,
        reverse: bool,
    ) -> std::collections::HashSet<u32> {
        let mut out = std::collections::HashSet::new();
        match path {
            PathExpr::Pred(iri) => {
                if let Some(pid) = self.path_pred_id(iri) {
                    if reverse {
                        out.extend(self.store.s_by_po(pid, start));
                    } else {
                        out.extend(self.store.o_by_sp(start, pid));
                    }
                }
            }
            PathExpr::Inverse(inner) => return self.path_reach(inner, start, !reverse),
            PathExpr::Seq(a, b) => {
                // forward: a then b; reverse: b then a (with reverse traversal).
                let (first, second) = if reverse { (b, a) } else { (a, b) };
                for mid in self.path_reach(first, start, reverse) {
                    out.extend(self.path_reach(second, mid, reverse));
                }
            }
            PathExpr::Alt(a, b) => {
                out.extend(self.path_reach(a, start, reverse));
                out.extend(self.path_reach(b, start, reverse));
            }
            PathExpr::ZeroOrOne(inner) => {
                out.insert(start);
                out.extend(self.path_reach(inner, start, reverse));
            }
            PathExpr::ZeroOrMore(inner) => {
                self.path_closure(inner, start, reverse, true, &mut out);
            }
            PathExpr::OneOrMore(inner) => {
                self.path_closure(inner, start, reverse, false, &mut out);
            }
            PathExpr::NegatedSet(preds) => {
                // One step via any predicate not in the negated set.
                let banned: std::collections::HashSet<u32> = preds
                    .iter()
                    .filter(|(_, inv)| *inv == reverse) // direction match
                    .filter_map(|(iri, _)| self.path_pred_id(iri))
                    .collect();
                if reverse {
                    for (p, s) in self.store.ps_by_o(start) {
                        if !banned.contains(&p) {
                            out.insert(s);
                        }
                    }
                } else {
                    for (p, o) in self.store.po_by_s(start) {
                        if !banned.contains(&p) {
                            out.insert(o);
                        }
                    }
                }
            }
        }
        out
    }

    /// Transitive closure for `*`/`+` via BFS over the inner path.
    fn path_closure(
        &self,
        inner: &PathExpr,
        start: u32,
        reverse: bool,
        reflexive: bool,
        out: &mut std::collections::HashSet<u32>,
    ) {
        let mut frontier = vec![start];
        let mut visited = std::collections::HashSet::new();
        if reflexive {
            out.insert(start);
            visited.insert(start);
        }
        while let Some(n) = frontier.pop() {
            for next in self.path_reach(inner, n, reverse) {
                if visited.insert(next) {
                    out.insert(next);
                    frontier.push(next);
                }
            }
        }
    }

    fn path_pred_id(&self, iri: &str) -> Option<PredId> {
        self.dict
            .predicate_id(&Term::iri(iri.to_string()).dict_key())
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
            GraphPattern::LeftJoin(a, b, filters) => {
                let left = self.eval_pattern(a, layout);
                let right = self.eval_pattern(b, layout);
                self.eval_left_join(left, b, right, filters, layout)
            }
            GraphPattern::Minus(a, b) => {
                let left = self.eval_pattern(a, layout);
                let right = self.eval_pattern(b, layout);
                let lvars = pattern_vars(a, layout);
                let rvars = pattern_vars(b, layout);
                eval_minus(left, &lvars, right, &rvars)
            }
            GraphPattern::Extend(inner, var, expr) => {
                let mut sols = self.eval_pattern(inner, layout);
                let vi = layout.index[var];
                for b in &mut sols {
                    // BIND error/unbound leaves the variable unbound (no row drop).
                    if let Some(v) = self.eval_value(expr, b, layout) {
                        b[vi] = Some(self.value_to_id(&v));
                    }
                }
                sols
            }
            GraphPattern::Values(vars, rows) => self.eval_values(vars, rows, layout),
            GraphPattern::SubSelect(sq) => self.eval_subselect(sq, layout),
            GraphPattern::Path(p) => self.eval_path_pattern(p, layout),
            GraphPattern::Graph(g, inner) => self.eval_graph(g, inner, layout),
            GraphPattern::Service {
                endpoint,
                silent,
                pattern,
            } => self.eval_service(endpoint, *silent, pattern, layout),
        }
    }

    /// Evaluate a `SERVICE [SILENT] <iri> { inner }` federated pattern.
    ///
    /// The inner pattern is serialized as a `SELECT * WHERE { … }` query, POSTed
    /// to the remote endpoint over HTTP, and the returned SPARQL-results JSON is
    /// decoded into bindings over this query's [`VarLayout`] (each returned term
    /// interned via the shared dictionary/extras so it joins with outer
    /// solutions). On any failure (variable endpoint, unserializable inner
    /// pattern, connection or parse error) a non-`SILENT` service yields no
    /// solutions, while a `SILENT` one yields the single identity solution so the
    /// outer query still returns.
    fn eval_service(
        &self,
        endpoint: &ServiceRef,
        silent: bool,
        inner: &GraphPattern,
        layout: &VarLayout,
    ) -> Vec<Binding> {
        // The identity solution: one all-unbound row (preserves outer bindings
        // under a join). Returned for SILENT failures.
        let identity = || vec![vec![None; layout.len()]];
        let fail = |silent: bool| -> Vec<Binding> {
            if silent {
                identity()
            } else {
                Vec::new()
            }
        };

        // Only constant-IRI endpoints are supported; a variable endpoint can't
        // be resolved without per-solution evaluation (uncommon — see backlog).
        let url = match endpoint {
            ServiceRef::Iri(iri) => iri.as_str(),
            ServiceRef::Var(_) => return fail(silent),
        };

        // Serialize the inner pattern to a remote SELECT query.
        let Some(query) = serialize_service_query(inner) else {
            return fail(silent);
        };

        // Fetch + parse the remote SPARQL-results JSON.
        let solutions = match crate::http_client::sparql_post(url, &query)
            .and_then(|body| crate::http_client::parse_sparql_json(&body))
        {
            Ok(sols) => sols,
            Err(_) => return fail(silent),
        };

        // Map each returned solution into this query's layout. Variables the
        // outer layout doesn't know about are ignored; the rest are interned to
        // ids so they join with outer solutions exactly like local bindings.
        solutions
            .into_iter()
            .map(|row| {
                let mut b = vec![None; layout.len()];
                for (var, term) in row {
                    if let Some(&idx) = layout.index.get(&var) {
                        b[idx] = Some(self.term_to_id(&term));
                    }
                }
                b
            })
            .collect()
    }

    /// Build a sub-evaluator over a named graph's store that shares this
    /// evaluator's `extras` interner, so computed terms (BIND/VALUES/aggregates)
    /// produced inside the `GRAPH` block resolve back here.
    fn graph_sub_evaluator<'b>(
        &'b self,
        gstore: &'a TripleStore,
        named: &'a BTreeMap<u32, TripleStore>,
    ) -> Evaluator<'a, TripleStore> {
        Evaluator {
            dict: self.dict,
            store: gstore,
            vstree: None,
            named: Some(named),
            extras: Rc::clone(&self.extras),
            plan_cache: std::cell::RefCell::new(HashMap::new()),
        }
    }

    /// Evaluate a `GRAPH (<iri> | ?g) { inner }` pattern. A constant graph runs
    /// `inner` against that named graph's store; a variable graph runs it against
    /// every named graph, binding the variable to each. The default graph is not
    /// visited (per SPARQL, `GRAPH ?g` ranges over named graphs only).
    fn eval_graph(&self, g: &GraphTerm, inner: &GraphPattern, layout: &VarLayout) -> Vec<Binding> {
        let Some(named) = self.named else {
            return Vec::new();
        };
        match g {
            GraphTerm::Iri(iri) => {
                let Some(gid) = self.dict.entity_id(&Term::iri(iri.clone()).dict_key()) else {
                    return Vec::new();
                };
                match named.get(&gid) {
                    Some(gstore) => self
                        .graph_sub_evaluator(gstore, named)
                        .eval_pattern(inner, layout),
                    None => Vec::new(),
                }
            }
            GraphTerm::Var(v) => {
                let vi = layout.index[v];
                let mut out = Vec::new();
                for (&gid, gstore) in named {
                    let sub = self.graph_sub_evaluator(gstore, named);
                    for mut b in sub.eval_pattern(inner, layout) {
                        b[vi] = Some(gid);
                        out.push(b);
                    }
                }
                out
            }
        }
    }

    /// OPTIONAL (left outer join). For each left solution, keep all compatible
    /// right solutions (after applying the OPTIONAL's inner FILTERs to the
    /// merged binding); if none, keep the left solution unextended.
    fn eval_left_join(
        &self,
        left: Vec<Binding>,
        b_pat: &GraphPattern,
        right: Vec<Binding>,
        filters: &[Expr],
        layout: &VarLayout,
    ) -> Vec<Binding> {
        let lvars: Vec<usize> = (0..layout.len()).collect();
        let rvars = pattern_vars(b_pat, layout);
        let _ = &lvars; // left may bind anything; merge handles compatibility
        let mut out = Vec::new();
        for l in &left {
            let mut matched = false;
            for r in &right {
                if let Some(m) = merge_bindings(l, r) {
                    if filters
                        .iter()
                        .all(|f| self.eval_ebv(f, &m, layout) == Some(true))
                    {
                        out.push(m);
                        matched = true;
                    }
                }
            }
            if !matched {
                out.push(l.clone());
            }
        }
        let _ = rvars;
        out
    }

    /// Evaluate an inline VALUES block into bindings over the query layout.
    fn eval_values(
        &self,
        vars: &[String],
        rows: &[Vec<Option<Term>>],
        layout: &VarLayout,
    ) -> Vec<Binding> {
        rows.iter()
            .map(|row| {
                let mut b = vec![None; layout.len()];
                for (vi, cell) in vars.iter().zip(row.iter()) {
                    if let (Some(&idx), Some(term)) = (layout.index.get(vi), cell.as_ref()) {
                        b[idx] = Some(self.term_to_id(term));
                    }
                }
                b
            })
            .collect()
    }

    /// Evaluate a sub-SELECT and lift its result rows into the outer layout.
    fn eval_subselect(&self, sq: &SelectQuery, layout: &VarLayout) -> Vec<Binding> {
        let (vars, _is_pred, rows) = match self.eval_select_solutions(sq) {
            Ok(t) => t,
            Err(_) => return Vec::new(),
        };
        // Map each sub-query result column to an outer layout slot (by name).
        let slots: Vec<Option<usize>> = vars.iter().map(|v| layout.index.get(v).copied()).collect();
        rows.into_iter()
            .map(|r| {
                let mut b = vec![None; layout.len()];
                for (col, id) in r.into_iter().enumerate() {
                    if let Some(idx) = slots[col] {
                        b[idx] = id;
                    }
                }
                b
            })
            .collect()
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

        // Exact per-variable candidate sets from constant edges (gStore's
        // FilterPlan/CompleteCandidate), optionally refined by the VS-tree.
        let mut candidates = candidates::generate(&plans, self.store);
        self.refine_candidates_with_vstree(&mut candidates, tps, layout);

        // Cost-based plan: optimal left-deep order, plus a binary-join (bushy)
        // tree when one is strictly cheaper. Memoised so a structurally-identical
        // BGP reuses the enumeration (gStore `plan_cache`).
        let key = plans_key(&plans);
        let plan = self
            .plan_cache
            .borrow_mut()
            .entry(key)
            .or_insert_with(|| optimizer::optimize(&plans, self.store, &candidates, layout.len()))
            .clone();

        if let Some(tree) = &plan.tree {
            // Bushy execution: build each side independently, then hash-join.
            let (sols, _vars) = self.eval_join_tree(tree, &plans, &candidates, layout);
            return sols;
        }

        // Left-deep pipeline: extend partial bindings one pattern at a time.
        let mut solutions: Vec<Binding> = vec![vec![None; layout.len()]];
        for &pi in &plan.order {
            let plan = &plans[pi];
            let mut next = Vec::new();
            for binding in &solutions {
                self.extend(binding, plan, &candidates, &mut next);
            }
            solutions = next;
            if solutions.is_empty() {
                break;
            }
        }
        solutions
    }

    /// Execute a binary-join tree: each `Leaf` scans its pattern from scratch and
    /// each `Join` hash-joins the two sub-results on their shared variables.
    /// Returns the bindings and the set of variable indices they bind.
    fn eval_join_tree(
        &self,
        tree: &JoinTree,
        plans: &[PatPlan],
        cands: &Candidates,
        layout: &VarLayout,
    ) -> (Vec<Binding>, Vec<usize>) {
        match tree {
            JoinTree::Leaf(i) => {
                let plan = &plans[*i];
                let empty = vec![None; layout.len()];
                let mut out = Vec::new();
                self.extend(&empty, plan, cands, &mut out);
                (out, optimizer::plan_vars(plan))
            }
            JoinTree::Join(l, r) => {
                let (lb, lv) = self.eval_join_tree(l, plans, cands, layout);
                if lb.is_empty() {
                    return (Vec::new(), lv);
                }
                let (rb, rv) = self.eval_join_tree(r, plans, cands, layout);
                let joined = join_sets(&lb, &lv, &rb, &rv);
                let mut vars = lv;
                for v in rv {
                    if !vars.contains(&v) {
                        vars.push(v);
                    }
                }
                (joined, vars)
            }
        }
    }

    /// Intersect the VS-tree's signature candidates into the exact candidate
    /// sets (both are sound supersets, so the intersection stays sound and only
    /// tightens). Applies to subject variables (always entities).
    fn refine_candidates_with_vstree(
        &self,
        candidates: &mut Candidates,
        tps: &[TriplePattern],
        layout: &VarLayout,
    ) {
        let Some(vstree) = self.vstree else {
            return;
        };
        let mut subj_vars: HashSet<usize> = HashSet::new();
        for tp in tps {
            if let PatternTerm::Var(v) = &tp.subject {
                subj_vars.insert(layout.index[v]);
            }
        }
        for &vidx in &subj_vars {
            let mut sig = Signature::new();
            for tp in tps {
                if matches!(&tp.subject, PatternTerm::Var(v) if layout.index[v] == vidx) {
                    sig.encode_query_edge(
                        self.pred_id_of(&tp.predicate),
                        self.neighbor_id_of(&tp.object),
                        EdgeDir::Out,
                    );
                }
                if matches!(&tp.object, PatternTerm::Var(v) if layout.index[v] == vidx) {
                    sig.encode_query_edge(
                        self.pred_id_of(&tp.predicate),
                        self.neighbor_id_of(&tp.subject),
                        EdgeDir::In,
                    );
                }
            }
            if sig.is_empty() {
                continue;
            }
            if let Some(mut vs) = vstree.candidates(&sig) {
                vs.sort_unstable();
                vs.dedup();
                match candidates.get_mut(&vidx) {
                    Some(existing) => existing.retain(|x| vs.binary_search(x).is_ok()),
                    None => {
                        candidates.insert(vidx, vs);
                    }
                }
            }
        }
    }

    /// The predicate id of a constant-IRI predicate position, if resolvable.
    fn pred_id_of(&self, pt: &PatternTerm) -> Option<PredId> {
        match pt {
            PatternTerm::Term(Term::Iri(iri)) => {
                self.dict.predicate_id(&Term::iri(iri.clone()).dict_key())
            }
            _ => None,
        }
    }

    /// The id of a constant neighbour (subject/object) position, if resolvable.
    fn neighbor_id_of(&self, pt: &PatternTerm) -> Option<u32> {
        match pt {
            PatternTerm::Term(t) => self.dict.term_id(t),
            PatternTerm::Var(_) => None,
        }
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
    /// `cands` (possibly empty) restricts entity variables to VS-tree candidates.
    fn extend(
        &self,
        binding: &Binding,
        plan: &PatPlan,
        cands: &Candidates,
        out: &mut Vec<Binding>,
    ) {
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
                .into_iter()
                .map(|t| (t.sub, t.pred, t.obj))
                .collect(),
        };

        for (s, p, o) in candidates {
            // VS-tree filter: an entity variable's value must be a candidate.
            if !cand_ok(&plan.s, s, cands) || !cand_ok(&plan.o, o, cands) {
                continue;
            }
            let mut nb = binding.clone();
            if unify(&mut nb, &plan.s, s)
                && unify(&mut nb, &plan.p, p)
                && unify(&mut nb, &plan.o, o)
            {
                out.push(nb);
            }
        }
    }

    // ---- value resolution -------------------------------------------------

    /// Resolve a bound id to its surface string for output.
    fn resolve_string(&self, id: Option<u32>, is_pred: bool) -> Option<String> {
        let id = id?;
        if is_synth(id) {
            return self.extras.borrow().term(id).map(Term::to_string);
        }
        if is_pred {
            self.dict.predicate_to_string(id).map(str::to_owned)
        } else {
            self.dict.id_to_string(id).map(str::to_owned)
        }
    }

    /// Resolve a variable to a runtime [`Value`] for expression evaluation.
    fn resolve_value(&self, idx: usize, binding: &Binding, layout: &VarLayout) -> Option<Value> {
        self.id_to_value(binding[idx]?, layout.is_pred[idx])
    }

    /// Resolve any id (dictionary or synthetic) to a runtime [`Value`].
    fn id_to_value(&self, id: u32, is_pred: bool) -> Option<Value> {
        if is_synth(id) {
            return self.extras.borrow().term(id).map(Value::from_term);
        }
        let s = if is_pred {
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
                        Value::Int(i) => Some(
                            i.checked_neg()
                                .map(Value::Int)
                                .unwrap_or_else(|| Value::Double(-(i as f64))),
                        ),
                        Value::Double(d) => Some(Value::Double(-d)),
                        _ => None,
                    },
                }
            }
            Expr::Builtin(name, args) => self.eval_builtin(name, args, b, l),
            Expr::Exists(neg, pat) => Some(Value::Bool(self.eval_exists(*neg, pat, b, l))),
            // Aggregates only have meaning in group context (handled elsewhere).
            Expr::Aggregate { .. } => None,
        }
    }

    /// `EXISTS { pat }` / `NOT EXISTS { pat }`: true iff `pat` has at least one
    /// solution compatible with the current binding `b`. We substitute `b`'s
    /// bound variables into `pat` as constants (so inner FILTER/BIND that
    /// reference outer variables see their values), then test non-emptiness.
    fn eval_exists(&self, negated: bool, pat: &GraphPattern, b: &Binding, outer: &VarLayout) -> bool {
        let bound = self.subst_pattern(pat, b, outer);
        let layout = VarLayout::build(&bound);
        let found = !self.eval_pattern(&bound, &layout).is_empty();
        found ^ negated
    }

    /// An id resolved back to its RDF term (synthetic, predicate, or
    /// entity/literal — routed by `is_pred` exactly as [`resolve_string`]).
    fn id_to_term(&self, id: u32, is_pred: bool) -> Option<Term> {
        let s = self.resolve_string(Some(id), is_pred)?;
        parse_term(&s).ok()
    }

    /// Clone `gp` with each variable bound by `b` replaced by its constant term
    /// (resolved in the variable's binding id-space via `layout.is_pred`). Used
    /// to inject the current solution into an EXISTS sub-pattern.
    fn subst_pattern(&self, gp: &GraphPattern, b: &Binding, layout: &VarLayout) -> GraphPattern {
        match gp {
            GraphPattern::Empty => GraphPattern::Empty,
            GraphPattern::Bgp(tps) => GraphPattern::Bgp(
                tps.iter()
                    .map(|tp| TriplePattern {
                        subject: self.subst_node(&tp.subject, b, layout),
                        predicate: self.subst_node(&tp.predicate, b, layout),
                        object: self.subst_node(&tp.object, b, layout),
                    })
                    .collect(),
            ),
            GraphPattern::Path(p) => GraphPattern::Path(PathPattern {
                subject: self.subst_node(&p.subject, b, layout),
                path: p.path.clone(),
                object: self.subst_node(&p.object, b, layout),
            }),
            GraphPattern::Join(a, c) => GraphPattern::Join(
                Box::new(self.subst_pattern(a, b, layout)),
                Box::new(self.subst_pattern(c, b, layout)),
            ),
            GraphPattern::Union(a, c) => GraphPattern::Union(
                Box::new(self.subst_pattern(a, b, layout)),
                Box::new(self.subst_pattern(c, b, layout)),
            ),
            GraphPattern::Minus(a, c) => GraphPattern::Minus(
                Box::new(self.subst_pattern(a, b, layout)),
                Box::new(self.subst_pattern(c, b, layout)),
            ),
            GraphPattern::LeftJoin(a, c, fs) => GraphPattern::LeftJoin(
                Box::new(self.subst_pattern(a, b, layout)),
                Box::new(self.subst_pattern(c, b, layout)),
                fs.iter().map(|e| self.subst_expr(e, b, layout)).collect(),
            ),
            GraphPattern::Filter(fs, inner) => GraphPattern::Filter(
                fs.iter().map(|e| self.subst_expr(e, b, layout)).collect(),
                Box::new(self.subst_pattern(inner, b, layout)),
            ),
            GraphPattern::Extend(inner, var, e) => GraphPattern::Extend(
                Box::new(self.subst_pattern(inner, b, layout)),
                var.clone(),
                self.subst_expr(e, b, layout),
            ),
            GraphPattern::Graph(g, inner) => {
                // Substitute a bound graph variable into a constant graph IRI.
                let g2 = match g {
                    GraphTerm::Var(name) => match layout
                        .index
                        .get(name)
                        .and_then(|&i| b[i].map(|id| (id, layout.is_pred[i])))
                        .and_then(|(id, p)| self.id_to_term(id, p))
                    {
                        Some(Term::Iri(s)) => GraphTerm::Iri(s),
                        _ => g.clone(),
                    },
                    GraphTerm::Iri(_) => g.clone(),
                };
                GraphPattern::Graph(g2, Box::new(self.subst_pattern(inner, b, layout)))
            }
            // Inline VALUES and sub-SELECT are kept intact (rare inside EXISTS).
            GraphPattern::Values(v, r) => GraphPattern::Values(v.clone(), r.clone()),
            GraphPattern::SubSelect(sq) => GraphPattern::SubSelect(sq.clone()),
            // SERVICE substitutes outer bindings into its inner pattern so the
            // remote query sees them as constants (its endpoint is unchanged).
            GraphPattern::Service {
                endpoint,
                silent,
                pattern,
            } => GraphPattern::Service {
                endpoint: endpoint.clone(),
                silent: *silent,
                pattern: Box::new(self.subst_pattern(pattern, b, layout)),
            },
        }
    }

    /// Replace a bound variable position with its constant term.
    fn subst_node(&self, pt: &PatternTerm, b: &Binding, layout: &VarLayout) -> PatternTerm {
        if let PatternTerm::Var(name) = pt {
            if let Some(&i) = layout.index.get(name) {
                if let Some(id) = b[i] {
                    if let Some(t) = self.id_to_term(id, layout.is_pred[i]) {
                        return PatternTerm::Term(t);
                    }
                }
            }
        }
        pt.clone()
    }

    /// Replace bound variable references inside an expression with constants.
    fn subst_expr(&self, e: &Expr, b: &Binding, layout: &VarLayout) -> Expr {
        match e {
            Expr::Var(name) => {
                if let Some(&i) = layout.index.get(name) {
                    if let Some(id) = b[i] {
                        if let Some(t) = self.id_to_term(id, layout.is_pred[i]) {
                            return Expr::Const(t);
                        }
                    }
                }
                e.clone()
            }
            Expr::Const(_) => e.clone(),
            Expr::Or(x, y) => Expr::Or(
                Box::new(self.subst_expr(x, b, layout)),
                Box::new(self.subst_expr(y, b, layout)),
            ),
            Expr::And(x, y) => Expr::And(
                Box::new(self.subst_expr(x, b, layout)),
                Box::new(self.subst_expr(y, b, layout)),
            ),
            Expr::Not(x) => Expr::Not(Box::new(self.subst_expr(x, b, layout))),
            Expr::Unary(op, x) => Expr::Unary(*op, Box::new(self.subst_expr(x, b, layout))),
            Expr::Compare(op, x, y) => Expr::Compare(
                *op,
                Box::new(self.subst_expr(x, b, layout)),
                Box::new(self.subst_expr(y, b, layout)),
            ),
            Expr::Arith(op, x, y) => Expr::Arith(
                *op,
                Box::new(self.subst_expr(x, b, layout)),
                Box::new(self.subst_expr(y, b, layout)),
            ),
            Expr::Builtin(n, args) => Expr::Builtin(
                n.clone(),
                args.iter().map(|a| self.subst_expr(a, b, layout)).collect(),
            ),
            Expr::Exists(neg, pat) => {
                Expr::Exists(*neg, Box::new(self.subst_pattern(pat, b, layout)))
            }
            Expr::Aggregate {
                func,
                distinct,
                arg,
                sep,
            } => Expr::Aggregate {
                func: *func,
                distinct: *distinct,
                arg: arg.as_ref().map(|a| Box::new(self.subst_expr(a, b, layout))),
                sep: sep.clone(),
            },
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
        compare_values(op, &a, &c)
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

/// Materialize a runtime [`Value`] as an RDF [`Term`].
fn value_to_term(v: &Value) -> Term {
    match v {
        Value::Iri(s) => Term::Iri(s.clone()),
        Value::Blank(s) => Term::Blank(s.clone()),
        Value::Int(i) => Term::typed_literal(i.to_string(), xsd::INTEGER),
        Value::Double(_) => Term::typed_literal(v.lexical(), xsd::DOUBLE),
        Value::Bool(bool_v) => Term::typed_literal(bool_v.to_string(), xsd::BOOLEAN),
        Value::Str { value, lang } => Term::Literal {
            value: value.clone(),
            datatype: None,
            lang: lang.clone(),
        },
        Value::Typed { value, datatype } => Term::Literal {
            value: value.clone(),
            datatype: Some(datatype.clone()),
            lang: None,
        },
    }
}

/// Compare two values with a SPARQL relational operator.
fn compare_values(op: CompareOp, a: &Value, b: &Value) -> Option<bool> {
    match op {
        CompareOp::Eq => a.sparql_eq(b),
        CompareOp::Ne => a.sparql_eq(b).map(|v| !v),
        CompareOp::Lt => a.sparql_cmp(b).map(|o| o.is_lt()),
        CompareOp::Gt => a.sparql_cmp(b).map(|o| o.is_gt()),
        CompareOp::Le => a.sparql_cmp(b).map(|o| o.is_le()),
        CompareOp::Ge => a.sparql_cmp(b).map(|o| o.is_ge()),
    }
}

/// Sum numeric values (integer-preserving until a double appears).
fn sum_values(vals: &[Value]) -> Value {
    let mut all_int = true;
    let mut isum: i64 = 0;
    let mut fsum: f64 = 0.0;
    for v in vals {
        match v {
            Value::Int(i) => {
                isum = isum.saturating_add(*i);
                fsum += *i as f64;
            }
            Value::Double(d) => {
                all_int = false;
                fsum += d;
            }
            _ => {}
        }
    }
    if all_int {
        Value::Int(isum)
    } else {
        Value::Double(fsum)
    }
}

/// Does a projection contain any aggregate (⇒ grouping evaluation)?
fn projection_has_aggregate(p: &Projection) -> bool {
    match p {
        Projection::All => false,
        Projection::Items(items) => items
            .iter()
            .any(|it| matches!(it, SelectItem::Expr(e, _) if expr_has_aggregate(e))),
    }
}

fn expr_has_aggregate(e: &Expr) -> bool {
    match e {
        Expr::Aggregate { .. } => true,
        Expr::Or(a, b) | Expr::And(a, b) | Expr::Arith(_, a, b) | Expr::Compare(_, a, b) => {
            expr_has_aggregate(a) || expr_has_aggregate(b)
        }
        Expr::Not(a) | Expr::Unary(_, a) => expr_has_aggregate(a),
        Expr::Builtin(_, args) => args.iter().any(expr_has_aggregate),
        _ => false,
    }
}

/// Bind a path endpoint (variable or constant) into a binding; false on conflict.
fn bind_node(b: &mut Binding, pt: &PatternTerm, id: u32, layout: &VarLayout) -> bool {
    match pt {
        PatternTerm::Var(v) => {
            let i = layout.index[v];
            match b[i] {
                Some(x) => x == id,
                None => {
                    b[i] = Some(id);
                    true
                }
            }
        }
        // Constant endpoints are pre-filtered to match in `path_pairs`.
        PatternTerm::Term(_) => true,
    }
}

/// MINUS: drop left solutions that have a compatible right solution sharing at
/// least one bound variable (SPARQL `MINUS` semantics).
fn eval_minus(
    left: Vec<Binding>,
    lvars: &[usize],
    right: Vec<Binding>,
    rvars: &[usize],
) -> Vec<Binding> {
    let shared: Vec<usize> = lvars
        .iter()
        .copied()
        .filter(|v| rvars.contains(v))
        .collect();
    if shared.is_empty() {
        return left; // no shared variables ⇒ MINUS removes nothing
    }
    left.into_iter()
        .filter(|l| {
            !right.iter().any(|r| {
                let mut compatible = true;
                let mut any_shared = false;
                for &v in &shared {
                    if let (Some(a), Some(b)) = (l[v], r[v]) {
                        any_shared = true;
                        if a != b {
                            compatible = false;
                            break;
                        }
                    }
                }
                compatible && any_shared
            })
        })
        .collect()
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

/// The variable indices a pattern structurally binds (incl. BIND/VALUES/sub-SELECT).
fn pattern_vars(gp: &GraphPattern, layout: &VarLayout) -> Vec<usize> {
    let mut names = Vec::new();
    gp.collect_vars(&mut names);
    let mut vars = Vec::new();
    for name in names {
        if let Some(&idx) = layout.index.get(&name) {
            if !vars.contains(&idx) {
                vars.push(idx);
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

/// A structural hash of compiled patterns, used as the plan-cache key. Two BGPs
/// with the same slot shape (same constants and same variable indices, in the
/// same order) optimize to the same plan, so they share a cache entry.
fn plans_key(plans: &[PatPlan]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    plans.len().hash(&mut h);
    for p in plans {
        for (tag, slot) in [(0u8, p.s), (1u8, p.p), (2u8, p.o)] {
            tag.hash(&mut h);
            match slot {
                Slot::Const(id) => {
                    0u8.hash(&mut h);
                    id.hash(&mut h);
                }
                Slot::Var(v) => {
                    1u8.hash(&mut h);
                    v.hash(&mut h);
                }
            }
        }
    }
    h.finish()
}

/// The known id at a slot given the current binding (const, or bound var).
fn slot_known(slot: &Slot, binding: &Binding) -> Option<u32> {
    match slot {
        Slot::Const(id) => Some(*id),
        Slot::Var(v) => binding[*v],
    }
}

/// Is `value` an allowed binding for `slot` under the VS-tree candidate sets?
/// Constants and variables without a candidate set are always allowed.
fn cand_ok(slot: &Slot, value: u32, cands: &Candidates) -> bool {
    match slot {
        // Candidate lists are sorted; membership is a binary search.
        Slot::Var(v) => cands
            .get(v)
            .is_none_or(|list| list.binary_search(&value).is_ok()),
        Slot::Const(_) => true,
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

/// Render a `SERVICE` inner pattern as a remote `SELECT * WHERE { … }` query
/// string. Supports the BGP subset shipped over federation — basic triple
/// patterns, conjunctions (`Join`), `UNION`, and empty groups. Returns `None`
/// for shapes a simple serializer can't faithfully reproduce (paths, OPTIONAL,
/// FILTER, BIND, VALUES, sub-SELECT, GRAPH, nested SERVICE), so the caller can
/// fall back gracefully.
fn serialize_service_query(inner: &GraphPattern) -> Option<String> {
    let mut body = String::new();
    serialize_pattern_body(inner, &mut body)?;
    Some(format!("SELECT * WHERE {{ {body}}}"))
}

/// Append a graph pattern's surface syntax to `out`; `None` if unsupported.
fn serialize_pattern_body(gp: &GraphPattern, out: &mut String) -> Option<()> {
    match gp {
        GraphPattern::Empty => Some(()),
        GraphPattern::Bgp(tps) => {
            for tp in tps {
                out.push_str(&render_pattern_term(&tp.subject));
                out.push(' ');
                out.push_str(&render_pattern_term(&tp.predicate));
                out.push(' ');
                out.push_str(&render_pattern_term(&tp.object));
                out.push_str(" . ");
            }
            Some(())
        }
        GraphPattern::Join(a, b) => {
            serialize_pattern_body(a, out)?;
            serialize_pattern_body(b, out)
        }
        GraphPattern::Union(a, b) => {
            out.push_str("{ ");
            serialize_pattern_body(a, out)?;
            out.push_str("} UNION { ");
            serialize_pattern_body(b, out)?;
            out.push_str("} ");
            Some(())
        }
        _ => None,
    }
}

/// Render a triple-pattern position as SPARQL: `?var` or its N-Triples term form
/// (which is itself valid SPARQL for IRIs/literals/blank nodes).
fn render_pattern_term(pt: &PatternTerm) -> String {
    match pt {
        PatternTerm::Var(v) => format!("?{v}"),
        PatternTerm::Term(t) => t.to_string(),
    }
}
