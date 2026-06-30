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
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;

use crate::analytics::GraphView;
use crate::dict::Dictionary;
use crate::error::{GStoreError, Result};
use crate::model::id::PredId;
use crate::model::{Term, Triple};
use crate::parser::ntriples::parse_term;
use crate::parser::sparql::ast::*;
use crate::signature::{EdgeDir, Signature, VsTree};
use crate::store::{TripleSource, TripleStore};

use super::candidates::{self, Candidates};
use super::functions::FunctionRegistry;
use super::hash;
use super::optimizer::{self, ExecPlan, JoinMethod, JoinTree};
use super::results::{QueryResult, ResultSet};
use super::value::{format_xsd_datetime_utc, order_key, parse_datetime_parts, DateTimeParts, Value};

/// `xsd:dateTime`, the datatype produced by `NOW()`.
const XSD_DATETIME: &str = "http://www.w3.org/2001/XMLSchema#dateTime";
/// `xsd:dayTimeDuration`, the datatype produced by `TIMEZONE()`.
const XSD_DAYTIMEDURATION: &str = "http://www.w3.org/2001/XMLSchema#dayTimeDuration";

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
    /// Lazily-built CSR adjacency view, shared across rows so the graph
    /// built-ins (`SHORTESTPATHLEN`, `KHOPREACHABLE`, `CYCLEBOOLEAN`, …) pay the
    /// O(E) build cost once per query rather than once per solution.
    graph_cache: std::cell::RefCell<Option<Rc<GraphView>>>,
    /// User-defined scalar functions (gStore's `pfnQuery`, but a safe in-process
    /// closure registry rather than dlopen/.so plugins). Consulted by the
    /// expression evaluator for names that are not SPARQL built-ins. Shared
    /// (`Rc`) with `GRAPH` sub-evaluators so they see the same functions.
    functions: Rc<FunctionRegistry>,
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
            graph_cache: std::cell::RefCell::new(None),
            functions: Rc::new(FunctionRegistry::new()),
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
            graph_cache: std::cell::RefCell::new(None),
            functions: Rc::new(FunctionRegistry::new()),
        }
    }

    /// Attach named graphs so `GRAPH` patterns can be evaluated.
    pub fn with_named(mut self, named: &'a BTreeMap<u32, TripleStore>) -> Evaluator<'a, S> {
        self.named = Some(named);
        self
    }

    /// Attach a registry of user-defined scalar functions (gStore `pfnQuery`,
    /// implemented as safe in-process closures instead of dlopen plugins). The
    /// expression evaluator consults it for any name that is not a SPARQL
    /// built-in. Built-ins always take precedence. Builder-style; chainable with
    /// [`with_named`](Self::with_named) / [`with_function`](Self::with_function).
    pub fn with_functions(mut self, functions: FunctionRegistry) -> Evaluator<'a, S> {
        self.functions = Rc::new(functions);
        self
    }

    /// Register a single user-defined scalar function (builder-style). See
    /// [`with_functions`](Self::with_functions). Equivalent to building a
    /// [`FunctionRegistry`], calling [`FunctionRegistry::register`], and passing
    /// it to `with_functions`.
    pub fn with_function<F>(mut self, name: &str, f: F) -> Evaluator<'a, S>
    where
        F: Fn(&[Value]) -> Option<Value> + Send + Sync + 'static,
    {
        self.register_function(name, f);
        self
    }

    /// Register a single user-defined scalar function on this evaluator
    /// in place. The name is case-insensitive (folded to upper case to match the
    /// SPARQL parser). See [`FunctionRegistry`] for the calling convention.
    pub fn register_function<F>(&mut self, name: &str, f: F) -> &mut Self
    where
        F: Fn(&[Value]) -> Option<Value> + Send + Sync + 'static,
    {
        Rc::make_mut(&mut self.functions).register(name, f);
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
        // COUNT(*) counts solutions; COUNT(DISTINCT *) counts the distinct
        // solution mappings (SPARQL 1.1 §18.5.1.1), so dedup whole bindings
        // before counting when the DISTINCT modifier is present.
        if func == AggFunc::Count && arg.is_none() {
            if distinct {
                let mut seen: HashSet<&Binding> = HashSet::new();
                let n = sols.iter().filter(|&&b| seen.insert(b)).count();
                return Some(Value::Int(n as i64));
            }
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
            graph_cache: std::cell::RefCell::new(None),
            functions: Rc::clone(&self.functions),
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
        let (vars, is_pred, rows) = match self.eval_select_solutions(sq) {
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
                        // A predicate-typed inner column carries a *predicate*-
                        // dictionary id, but the outer layout may not know this
                        // slot is predicate-typed (`collect_triples` does not
                        // recurse into sub-SELECTs), so resolving it later via the
                        // entity/literal dictionary would be wrong. Re-mint such a
                        // value as a synthetic term, which resolves correctly
                        // regardless of the outer `is_pred` flag and stays
                        // join-consistent (the interner is idempotent per term).
                        b[idx] = match id {
                            Some(pid) if is_pred[col] && !is_synth(pid) => self
                                .id_to_term(pid, true)
                                .map(|t| self.extras.borrow_mut().intern(&t)),
                            other => other,
                        };
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

            // ---- strings (SPARQL 1.1) ------------------------------------
            "SUBSTR" => {
                let src = self.eval_value(args.first()?, b, l)?;
                let (s, lang) = str_and_lang(&src);
                let chars: Vec<char> = s.chars().collect();
                let total = chars.len() as i64;
                let start = self.eval_value(args.get(1)?, b, l)?.as_f64()?.round() as i64;
                // XPath fn:substring semantics: keep 1-based positions `p` with
                // start <= p < start+length (length defaults to the rest).
                let end = match args.get(2) {
                    Some(e) => start + self.eval_value(e, b, l)?.as_f64()?.round() as i64,
                    None => total + 1,
                };
                let from = (start.max(1) - 1).min(total) as usize;
                let to = (end.max(1) - 1).clamp(0, total) as usize;
                let value = if to > from {
                    chars[from..to].iter().collect()
                } else {
                    String::new()
                };
                Some(Value::Str { value, lang })
            }
            "REPLACE" => {
                let src = self.eval_value(args.first()?, b, l)?;
                let (text, lang) = str_and_lang(&src);
                let pat = self.eval_value(args.get(1)?, b, l)?.lexical();
                let rep = self.eval_value(args.get(2)?, b, l)?.lexical();
                let flags = match args.get(3) {
                    Some(a) => self.eval_value(a, b, l)?.lexical(),
                    None => String::new(),
                };
                let pat = if flags.contains('i') {
                    format!("(?i){pat}")
                } else {
                    pat
                };
                let re = Regex::new(&pat).ok()?;
                Some(Value::Str {
                    value: re.replace_all(&text, rep.as_str()).into_owned(),
                    lang,
                })
            }
            "ENCODE_FOR_URI" => {
                let s = self.eval_value(args.first()?, b, l)?.lexical();
                Some(Value::Str {
                    value: encode_for_uri(&s),
                    lang: None,
                })
            }
            "STRLANG" => {
                let s = self.eval_value(args.first()?, b, l)?.lexical();
                let lang = self.eval_value(args.get(1)?, b, l)?.lexical();
                Some(Value::Str {
                    value: s,
                    lang: Some(lang),
                })
            }
            "STRDT" => {
                let s = self.eval_value(args.first()?, b, l)?.lexical();
                let dt = match self.eval_value(args.get(1)?, b, l)? {
                    Value::Iri(iri) => iri,
                    other => other.lexical(),
                };
                Some(Value::from_term(&Term::Literal {
                    value: s,
                    datatype: Some(dt),
                    lang: None,
                }))
            }
            "LANGMATCHES" => {
                let tag = self.eval_value(args.first()?, b, l)?.lexical();
                let range = self.eval_value(args.get(1)?, b, l)?.lexical();
                Some(Value::Bool(lang_matches(&tag, &range)))
            }
            "SIMILARITY" => {
                let x = self.eval_value(args.first()?, b, l)?.lexical();
                let y = self.eval_value(args.get(1)?, b, l)?.lexical();
                Some(Value::Double(jaro_similarity(&x, &y)))
            }

            // ---- numeric -------------------------------------------------
            "RAND" => Some(Value::Double(next_rand())),

            // ---- date / time ---------------------------------------------
            "NOW" => Some(Value::Typed {
                value: format_xsd_datetime_utc(now_unix_secs()),
                datatype: XSD_DATETIME.to_string(),
            }),
            "YEAR" => self.datetime_field(args, b, l, |p| p.year),
            "MONTH" => self.datetime_field(args, b, l, |p| p.month),
            "DAY" => self.datetime_field(args, b, l, |p| p.day),
            "HOURS" => self.datetime_field(args, b, l, |p| p.hour),
            "MINUTES" => self.datetime_field(args, b, l, |p| p.minute),
            "SECONDS" => {
                let p = parse_datetime_parts(&self.eval_value(args.first()?, b, l)?.lexical())?;
                if p.frac == 0.0 {
                    Some(Value::Int(p.second))
                } else {
                    Some(Value::Double(p.second as f64 + p.frac))
                }
            }
            "TIMEZONE" => {
                let p = parse_datetime_parts(&self.eval_value(args.first()?, b, l)?.lexical())?;
                // No timezone ⇒ error (per SPARQL); excludes the solution.
                let off = p.tz_offset_secs?;
                Some(Value::Typed {
                    value: day_time_duration(off),
                    datatype: XSD_DAYTIMEDURATION.to_string(),
                })
            }
            "TZ" => {
                let p = parse_datetime_parts(&self.eval_value(args.first()?, b, l)?.lexical())?;
                let value = match p.tz_offset_secs {
                    None => String::new(),
                    Some(0) => "Z".to_string(),
                    Some(off) => format_tz(off),
                };
                Some(Value::Str { value, lang: None })
            }

            // ---- constructors / terms ------------------------------------
            "IRI" | "URI" => match self.eval_value(args.first()?, b, l)? {
                Value::Iri(s) => Some(Value::Iri(s)),
                other => Some(Value::Iri(other.lexical())),
            },
            "BNODE" => {
                let label = match args.first() {
                    Some(a) => format!("b{:016x}", fnv1a(&self.eval_value(a, b, l)?.lexical())),
                    None => format!("b{}", next_bnode_id()),
                };
                Some(Value::Blank(label))
            }
            "UUID" => Some(Value::Iri(format!("urn:uuid:{}", gen_uuid()))),
            "STRUUID" => Some(Value::Str {
                value: gen_uuid(),
                lang: None,
            }),
            "IF" => {
                if args.len() != 3 {
                    return None;
                }
                if self.eval_ebv(&args[0], b, l)? {
                    self.eval_value(&args[1], b, l)
                } else {
                    self.eval_value(&args[2], b, l)
                }
            }

            // ---- hashing -------------------------------------------------
            "MD5" => self.hash1(args, b, l, hash::md5_hex),
            "SHA1" => self.hash1(args, b, l, hash::sha1_hex),
            "SHA256" => self.hash1(args, b, l, hash::sha256_hex),
            "SHA384" => self.hash1(args, b, l, hash::sha384_hex),
            "SHA512" => self.hash1(args, b, l, hash::sha512_hex),

            // ---- gStore graph built-ins ----------------------------------
            "CYCLEBOOLEAN" | "SIMPLECYCLEBOOLEAN" => {
                Some(Value::Bool(self.graph_view().has_cycle()))
            }
            "SHORTESTPATHLEN" => {
                let a = self.node_id(args.first()?, b, l)?;
                let c = self.node_id(args.get(1)?, b, l)?;
                let len = self.graph_view().shortest_path_len(a, c);
                // Unreachable ⇒ -1 sentinel (documented), reachable ⇒ #edges.
                Some(Value::Int(len.map(|x| x as i64).unwrap_or(-1)))
            }
            "KHOPREACHABLE" => {
                let a = self.node_id(args.first()?, b, l)?;
                let c = self.node_id(args.get(1)?, b, l)?;
                let kf = self.eval_value(args.get(2)?, b, l)?.as_f64()?;
                let k = if kf < 0.0 { u32::MAX } else { kf as u32 };
                Some(Value::Bool(
                    self.graph_view().khop_reachable(a, c, k).unwrap_or(false),
                ))
            }

            // User-defined function fallback (gStore `pfnQuery`): a name that is
            // not a SPARQL built-in is looked up in the registry before erroring.
            // Arguments are evaluated to values first; if any errors/unbound the
            // call yields `None`, like a built-in with a bad argument.
            _ => {
                let f = self.functions.get(name)?;
                let mut argv = Vec::with_capacity(args.len());
                for a in args {
                    argv.push(self.eval_value(a, b, l)?);
                }
                f(&argv)
            }
        }
    }

    /// Resolve a graph built-in node argument to its dictionary entity id.
    /// `None` when the term is not in the store (so it cannot be a graph node).
    fn node_id(&self, e: &Expr, b: &Binding, l: &VarLayout) -> Option<u32> {
        let v = self.eval_value(e, b, l)?;
        self.dict.term_id(&value_to_term(&v))
    }

    /// The lazily-built, query-scoped CSR adjacency view backing graph built-ins.
    fn graph_view(&self) -> Rc<GraphView> {
        if let Some(g) = self.graph_cache.borrow().as_ref() {
            return Rc::clone(g);
        }
        let g = Rc::new(GraphView::from_source(self.store));
        *self.graph_cache.borrow_mut() = Some(Rc::clone(&g));
        g
    }

    /// Extract an integer field from an `xsd:dateTime` argument.
    fn datetime_field(
        &self,
        args: &[Expr],
        b: &Binding,
        l: &VarLayout,
        f: impl Fn(&DateTimeParts) -> i64,
    ) -> Option<Value> {
        let p = parse_datetime_parts(&self.eval_value(args.first()?, b, l)?.lexical())?;
        Some(Value::Int(f(&p)))
    }

    /// Hash an argument's lexical form to a lowercase-hex string literal.
    fn hash1(
        &self,
        args: &[Expr],
        b: &Binding,
        l: &VarLayout,
        f: impl Fn(&str) -> String,
    ) -> Option<Value> {
        let s = self.eval_value(args.first()?, b, l)?.lexical();
        Some(Value::Str {
            value: f(&s),
            lang: None,
        })
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

/// A value's lexical form plus its language tag (preserved by `SUBSTR`/`REPLACE`
/// when the input is a language-tagged string literal).
fn str_and_lang(v: &Value) -> (String, Option<String>) {
    match v {
        Value::Str { value, lang } => (value.clone(), lang.clone()),
        other => (other.lexical(), None),
    }
}

/// Percent-encode a string for `ENCODE_FOR_URI`: every byte except the URI
/// unreserved set (`A-Z a-z 0-9 - _ . ~`) becomes `%XX` (uppercase hex).
fn encode_for_uri(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for &byte in s.as_bytes() {
        let unreserved = byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~');
        if unreserved {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    out
}

/// RFC 4647 basic language-range matching for `LANGMATCHES`. Range `"*"` matches
/// any non-empty tag; otherwise a case-insensitive equality or `range-` prefix.
fn lang_matches(tag: &str, range: &str) -> bool {
    if range == "*" {
        return !tag.is_empty();
    }
    if tag.eq_ignore_ascii_case(range) {
        return true;
    }
    let tag = tag.to_ascii_lowercase();
    let range = range.to_ascii_lowercase();
    tag.starts_with(&format!("{range}-"))
}

/// Format a timezone offset (seconds east of UTC) as `±HH:MM` for `TZ`.
fn format_tz(off: i64) -> String {
    let sign = if off < 0 { '-' } else { '+' };
    let a = off.abs();
    format!("{}{:02}:{:02}", sign, a / 3600, (a % 3600) / 60)
}

/// Format a timezone offset (seconds east of UTC) as an `xsd:dayTimeDuration`
/// lexical (`PT0S`, `-PT5H`, `PT1H30M`, …) for `TIMEZONE`.
fn day_time_duration(off: i64) -> String {
    if off == 0 {
        return "PT0S".to_string();
    }
    let sign = if off < 0 { "-" } else { "" };
    let a = off.abs();
    let (h, m) = (a / 3600, (a % 3600) / 60);
    let mut s = format!("{sign}PT");
    if h > 0 {
        s.push_str(&format!("{h}H"));
    }
    if m > 0 {
        s.push_str(&format!("{m}M"));
    }
    s
}

/// Current wall-clock time as whole seconds since the Unix epoch (UTC).
fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 64-bit FNV-1a hash — a stable label source for `BNODE("x")`.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &byte in s.as_bytes() {
        h ^= byte as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Monotonic counter for argument-less `BNODE()` labels.
static BNODE_COUNTER: AtomicU64 = AtomicU64::new(0);
fn next_bnode_id() -> u64 {
    BNODE_COUNTER.fetch_add(1, AtomicOrdering::Relaxed)
}

/// State for a tiny non-cryptographic PRNG (xorshift64*) backing `RAND`, `UUID`,
/// and `STRUUID`. Seeded lazily from the system clock. NOT suitable for any
/// security purpose — it exists only so these built-ins return varied values.
static RNG_STATE: AtomicU64 = AtomicU64::new(0);

fn rng_next_u64() -> u64 {
    let mut s = RNG_STATE.load(AtomicOrdering::Relaxed);
    if s == 0 {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15);
        s = seed | 1; // never zero
    }
    s ^= s >> 12;
    s ^= s << 25;
    s ^= s >> 27;
    RNG_STATE.store(s, AtomicOrdering::Relaxed);
    s.wrapping_mul(0x2545F4914F6CDD1D)
}

/// A pseudo-random double in `[0, 1)` for `RAND` (non-cryptographic).
fn next_rand() -> f64 {
    // 53 significant bits → uniform in [0, 1).
    (rng_next_u64() >> 11) as f64 / ((1u64 << 53) as f64)
}

/// Generate a random (version-4-shaped) UUID string for `UUID`/`STRUUID`. The
/// randomness is the non-cryptographic PRNG above, so this is a convenience
/// identifier, not a guaranteed-unique or secure UUID.
fn gen_uuid() -> String {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&rng_next_u64().to_le_bytes());
    bytes[8..].copy_from_slice(&rng_next_u64().to_le_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // RFC 4122 variant
    let h = |b: u8| format!("{b:02x}");
    format!(
        "{}{}{}{}-{}{}-{}{}-{}{}-{}{}{}{}{}{}",
        h(bytes[0]), h(bytes[1]), h(bytes[2]), h(bytes[3]),
        h(bytes[4]), h(bytes[5]), h(bytes[6]), h(bytes[7]),
        h(bytes[8]), h(bytes[9]), h(bytes[10]), h(bytes[11]),
        h(bytes[12]), h(bytes[13]), h(bytes[14]), h(bytes[15]),
    )
}

/// Jaro string similarity in `[0, 1]`, backing the gStore `SIMILARITY` built-in
/// (ported from `TempResult::doSimilarity`, including its equal/empty/substring
/// short-cuts so results track the C++ engine).
fn jaro_similarity(s: &str, t: &str) -> f64 {
    if s == t {
        return 1.0;
    }
    let sb: Vec<char> = s.chars().collect();
    let tb: Vec<char> = t.chars().collect();
    let (s_len, t_len) = (sb.len(), tb.len());
    if s_len == 0 || t_len == 0 {
        return 0.0;
    }
    if s_len < t_len {
        return jaro_similarity(t, s); // keep `s` the longer string
    }
    if s.contains(t) {
        return (t_len as f64 / s_len as f64 + 2.0) / 3.0;
    }
    let match_dist = t_len as i64 / 2 - 1; // gStore's window: floor(t_len/2) - 1
    let mut s_match = vec![false; s_len];
    let mut t_match = vec![false; t_len];
    let mut m = 0usize;
    for (i, &sc) in sb.iter().enumerate() {
        let lo = (i as i64 - match_dist).max(0) as usize;
        let hi = ((i as i64 + match_dist).min(t_len as i64 - 1)).max(-1);
        if hi < 0 {
            continue;
        }
        for j in lo..=(hi as usize) {
            if !t_match[j] && sc == tb[j] {
                s_match[i] = true;
                t_match[j] = true;
                m += 1;
                break;
            }
        }
    }
    if m == 0 {
        return 0.0;
    }
    let mut k = 0usize;
    let mut trans = 0usize;
    for (i, &matched) in s_match.iter().enumerate() {
        if matched {
            while k < t_len && !t_match[k] {
                k += 1;
            }
            if k < t_len {
                if sb[i] != tb[k] {
                    trans += 1;
                }
                k += 1;
            }
        }
    }
    let trans = (trans / 2) as f64;
    let m = m as f64;
    (m / s_len as f64 + m / t_len as f64 + (m - trans) / m) / 3.0
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
///
/// The physical method is chosen *per pair of operands* by [`optimizer::judge`]
/// (a port of gStore `Join::judge`): a [`JoinMethod::NestedLoop`] when one side
/// is tiny / highly selective, otherwise a [`JoinMethod::Hash`]. Both methods
/// emit solutions in the same order (left-major, right-minor), so the choice
/// never changes the result — only the cost. A cross product is used when there
/// is no usable shared key.
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

    // Cross product when there is no shared variable, OR when a shared variable
    // is left *unbound* (None) on some row: SPARQL compatibility treats an
    // unbound value as a wildcard that matches anything, which the hash key
    // (keyed on `Option<u32>`, where `None` is a distinct key) cannot express.
    // This arises with OPTIONAL results re-joined later and with a SILENT
    // SERVICE's identity solution. `merge_bindings` does the correct per-pair
    // compatible-merge (None acts as a wildcard).
    let unbound_shared = shared
        .iter()
        .any(|&i| left.iter().chain(right.iter()).any(|b| b[i].is_none()));
    if shared.is_empty() || unbound_shared {
        let mut out = Vec::new();
        for l in left {
            for r in right {
                if let Some(m) = merge_bindings(l, r) {
                    out.push(m);
                }
            }
        }
        return out;
    }

    match optimizer::judge(left.len(), right.len()) {
        JoinMethod::Hash => join_sets_hash(left, right, &shared),
        JoinMethod::NestedLoop => join_sets_nested(left, right, &shared),
    }
}

/// Hash join on the shared-variable key (gStore `multi_join`): build a hash
/// table on the right side, then probe it once per left row.
fn join_sets_hash(left: &[Binding], right: &[Binding], shared: &[usize]) -> Vec<Binding> {
    let key_of = |b: &Binding| -> Vec<Option<u32>> { shared.iter().map(|&i| b[i]).collect() };
    let mut index: HashMap<Vec<Option<u32>>, Vec<&Binding>> = HashMap::new();
    for r in right {
        index.entry(key_of(r)).or_default().push(r);
    }
    let mut out = Vec::new();
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

/// Nested-loop join (gStore `index_join`): for each left row, scan the right
/// side directly, relying on `merge_bindings` to reject incompatible pairs. No
/// hash table is built, which is cheapest when one side is tiny. Emits in the
/// same left-major, right-minor order as [`join_sets_hash`].
fn join_sets_nested(left: &[Binding], right: &[Binding], shared: &[usize]) -> Vec<Binding> {
    let mut out = Vec::new();
    for l in left {
        for r in right {
            // Fast reject on the shared key before the full compatible-merge.
            if shared.iter().all(|&i| l[i] == r[i]) {
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

#[cfg(test)]
mod builtin_tests {
    use super::*;
    use crate::model::IdTriple;
    use crate::parser::sparql::parse;

    const DT: &str = "http://www.w3.org/2001/XMLSchema#dateTime";
    /// An `xsd:dateTime` literal in SPARQL surface syntax, used as a constant in
    /// the date-accessor tests (2011-01-10 14:45:13.815, UTC-5).
    const DTL: &str =
        "\"2011-01-10T14:45:13.815-05:00\"^^<http://www.w3.org/2001/XMLSchema#dateTime>";

    /// Path `a→b→c→d` (acyclic) with `a name "Hello World"` and `a born <dt>`;
    /// when `cyclic`, add `d→a` to close the loop.
    fn fixture(cyclic: bool) -> (Dictionary, TripleStore) {
        let mut d = Dictionary::new();
        let a = d.intern_term(&Term::iri("http://ex/a"));
        let b = d.intern_term(&Term::iri("http://ex/b"));
        let c = d.intern_term(&Term::iri("http://ex/c"));
        let dd = d.intern_term(&Term::iri("http://ex/d"));
        let p = d.intern_predicate(&Term::iri("http://ex/p").dict_key());
        let name = d.intern_predicate(&Term::iri("http://ex/name").dict_key());
        let born = d.intern_predicate(&Term::iri("http://ex/born").dict_key());
        let hello = d.intern_term(&Term::plain_literal("Hello World"));
        let dtv = d.intern_term(&Term::typed_literal("2011-01-10T14:45:13.815-05:00", DT));
        let mut s = TripleStore::new();
        let mut triples = vec![
            IdTriple::new(a, p, b),
            IdTriple::new(b, p, c),
            IdTriple::new(c, p, dd),
            IdTriple::new(a, name, hello),
            IdTriple::new(a, born, dtv),
        ];
        if cyclic {
            triples.push(IdTriple::new(dd, p, a));
        }
        s.bulk_load(triples);
        (d, s)
    }

    /// Evaluate `(expr AS ?r)` over a one-row pattern; return the rendered cell.
    fn scalar(expr: &str) -> String {
        scalar_on(false, expr)
    }

    fn scalar_on(cyclic: bool, expr: &str) -> String {
        let (d, s) = fixture(cyclic);
        let q = format!("SELECT ({expr} AS ?r) WHERE {{ <http://ex/a> <http://ex/name> ?n }}");
        let parsed = parse(&q).unwrap();
        let rs = match Evaluator::new(&d, &s).evaluate(&parsed).unwrap() {
            QueryResult::Select(rs) => rs,
            other => panic!("expected SELECT, got {other:?}"),
        };
        assert_eq!(rs.row_count(), 1, "expected one row evaluating `{expr}`");
        rs.rows[0][0]
            .clone()
            .unwrap_or_else(|| panic!("`{expr}` produced an unbound result"))
    }

    /// The lexical value inside a rendered literal `"value"^^<dt>` / `"value"@l`.
    fn lit(rendered: &str) -> String {
        let body = rendered.strip_prefix('"').unwrap_or(rendered);
        match body.find('"') {
            Some(i) => body[..i].to_string(),
            None => body.to_string(),
        }
    }

    /// How many solutions a WHERE pattern (with FILTERs) yields.
    fn count(where_clause: &str) -> usize {
        let (d, s) = fixture(false);
        let q = format!("SELECT * WHERE {{ {where_clause} }}");
        let parsed = parse(&q).unwrap();
        match Evaluator::new(&d, &s).evaluate(&parsed).unwrap() {
            QueryResult::Select(rs) => rs.row_count(),
            other => panic!("expected SELECT, got {other:?}"),
        }
    }

    // ---- string functions -------------------------------------------------

    #[test]
    fn substr_one_and_two_arg() {
        assert_eq!(scalar(r#"SUBSTR("foobar", 4)"#), r#""bar""#);
        assert_eq!(scalar(r#"SUBSTR("foobar", 4, 2)"#), r#""ba""#);
        // XPath edge cases: start < 1 and over-long length clamp.
        assert_eq!(scalar(r#"SUBSTR("metadata", 4, 3)"#), r#""ada""#);
        assert_eq!(scalar(r#"SUBSTR("abc", -2, 4)"#), r#""a""#);
        // language tag is preserved.
        assert_eq!(scalar(r#"SUBSTR("chat"@fr, 1, 2)"#), r#""ch"@fr"#);
    }

    #[test]
    fn replace_plain_and_regex() {
        assert_eq!(scalar(r#"REPLACE("abcabc", "b", "X")"#), r#""aXcaXc""#);
        assert_eq!(scalar(r#"REPLACE("Hello", "[aeiou]", "_", "i")"#), r#""H_ll_""#);
        // group reference $1.
        assert_eq!(
            scalar(r#"REPLACE("2024-06-30", "(\\d+)-(\\d+)-(\\d+)", "$3/$2/$1")"#),
            r#""30/06/2024""#
        );
    }

    #[test]
    fn encode_for_uri_percent_encodes() {
        assert_eq!(scalar(r#"ENCODE_FOR_URI("a b/c?")"#), r#""a%20b%2Fc%3F""#);
        assert_eq!(scalar(r#"ENCODE_FOR_URI("keep-_.~")"#), r#""keep-_.~""#);
    }

    #[test]
    fn strlang_and_strdt() {
        assert_eq!(scalar(r#"STRLANG("chat", "en")"#), r#""chat"@en"#);
        assert_eq!(
            scalar(r#"STRDT("42", <http://www.w3.org/2001/XMLSchema#integer>)"#),
            r#""42"^^<http://www.w3.org/2001/XMLSchema#integer>"#
        );
    }

    #[test]
    fn langmatches_basic_ranges() {
        assert_eq!(lit(&scalar(r#"LANGMATCHES("en-US", "en")"#)), "true");
        assert_eq!(lit(&scalar(r#"LANGMATCHES("EN", "en")"#)), "true");
        assert_eq!(lit(&scalar(r#"LANGMATCHES("fr", "en")"#)), "false");
        assert_eq!(lit(&scalar(r#"LANGMATCHES("en", "*")"#)), "true");
        assert_eq!(lit(&scalar(r#"LANGMATCHES("", "*")"#)), "false");
    }

    #[test]
    fn similarity_jaro() {
        assert_eq!(lit(&scalar(r#"SIMILARITY("abc", "abc")"#)), "1.0");
        assert_eq!(lit(&scalar(r#"SIMILARITY("abc", "xyz")"#)), "0.0");
        // "abc" is a substring of "abcdef": (3/6 + 2)/3 = 0.8333…
        let sub: f64 = lit(&scalar(r#"SIMILARITY("abcdef", "abc")"#)).parse().unwrap();
        assert!((sub - 0.833_333).abs() < 1e-4, "got {sub}");
    }

    // ---- numeric ----------------------------------------------------------

    #[test]
    fn rand_in_unit_interval() {
        for _ in 0..20 {
            let r: f64 = lit(&scalar("RAND()")).parse().unwrap();
            assert!((0.0..1.0).contains(&r), "RAND() out of range: {r}");
        }
    }

    // ---- date / time ------------------------------------------------------

    #[test]
    fn datetime_accessors() {
        assert_eq!(
            scalar(&format!("YEAR({DTL})")),
            r#""2011"^^<http://www.w3.org/2001/XMLSchema#integer>"#
        );
        assert_eq!(lit(&scalar(&format!("MONTH({DTL})"))), "1");
        assert_eq!(lit(&scalar(&format!("DAY({DTL})"))), "10");
        assert_eq!(lit(&scalar(&format!("HOURS({DTL})"))), "14");
        assert_eq!(lit(&scalar(&format!("MINUTES({DTL})"))), "45");
        // fractional seconds → xsd:double
        assert_eq!(lit(&scalar(&format!("SECONDS({DTL})"))), "13.815");
    }

    #[test]
    fn timezone_and_tz() {
        assert_eq!(
            scalar(&format!("TIMEZONE({DTL})")),
            r#""-PT5H"^^<http://www.w3.org/2001/XMLSchema#dayTimeDuration>"#
        );
        assert_eq!(scalar(&format!("TZ({DTL})")), r#""-05:00""#);
        // A zoneless dateTime has an empty TZ.
        assert_eq!(
            scalar(r#"TZ("2020-01-01T00:00:00"^^<http://www.w3.org/2001/XMLSchema#dateTime>)"#),
            r#""""#
        );
    }

    #[test]
    fn now_returns_a_recent_datetime() {
        let year: i64 = lit(&scalar("YEAR(NOW())")).parse().unwrap();
        assert!(year >= 2020, "NOW() year looks wrong: {year}");
    }

    // ---- constructors / terms --------------------------------------------

    #[test]
    fn iri_and_uri_constructors() {
        assert_eq!(scalar(r#"IRI("http://ex/z")"#), "<http://ex/z>");
        assert_eq!(scalar(r#"URI("http://ex/z")"#), "<http://ex/z>");
        // IRI of an IRI is the IRI unchanged.
        assert_eq!(scalar("IRI(<http://ex/q>)"), "<http://ex/q>");
    }

    #[test]
    fn bnode_uuid_struuid_shapes() {
        assert!(scalar("BNODE()").starts_with("_:"));
        assert!(scalar(r#"BNODE("seed")"#).starts_with("_:b"));
        let uuid = scalar("UUID()");
        assert!(uuid.starts_with("<urn:uuid:") && uuid.ends_with('>'), "{uuid}");
        let struuid = lit(&scalar("STRUUID()"));
        assert_eq!(struuid.len(), 36);
        assert_eq!(struuid.matches('-').count(), 4);
        // version-4 nibble.
        assert_eq!(&struuid[14..15], "4");
    }

    // ---- hashing ----------------------------------------------------------

    #[test]
    fn hash_builtins_match_known_vectors() {
        assert_eq!(lit(&scalar(r#"MD5("abc")"#)), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            lit(&scalar(r#"SHA1("abc")"#)),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            lit(&scalar(r#"SHA256("abc")"#)),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(lit(&scalar(r#"SHA384("abc")"#)).len(), 96);
        assert_eq!(lit(&scalar(r#"SHA512("abc")"#)).len(), 128);
    }

    // ---- IF ---------------------------------------------------------------

    #[test]
    fn if_selects_branch_lazily() {
        assert_eq!(scalar(r#"IF(2 > 1, "yes", "no")"#), r#""yes""#);
        assert_eq!(scalar(r#"IF(2 < 1, "yes", "no")"#), r#""no""#);
        // The untaken branch is not evaluated, so its error does not propagate.
        assert_eq!(scalar(r#"IF(2 > 1, "ok", 1/0)"#), r#""ok""#);
    }

    // ---- FILTER integration ----------------------------------------------

    #[test]
    fn new_builtins_work_inside_filter() {
        assert_eq!(
            count(r#"<http://ex/a> <http://ex/name> ?n FILTER(STRSTARTS(SUBSTR(?n,1,5), "Hello"))"#),
            1
        );
        assert_eq!(
            count(r#"<http://ex/a> <http://ex/name> ?n FILTER(LANGMATCHES("en", "*") && IF(1>0, true, false))"#),
            1
        );
        assert_eq!(
            count(r#"<http://ex/a> <http://ex/name> ?n FILTER(SIMILARITY(?n, "zzz") > 0.9)"#),
            0
        );
    }

    // ---- gStore graph built-ins ------------------------------------------

    #[test]
    fn cycleboolean_detects_directed_cycle() {
        assert_eq!(lit(&scalar_on(false, "CYCLEBOOLEAN()")), "false");
        assert_eq!(lit(&scalar_on(true, "CYCLEBOOLEAN()")), "true");
        assert_eq!(lit(&scalar_on(true, "SIMPLECYCLEBOOLEAN()")), "true");
    }

    #[test]
    fn shortestpathlen_counts_edges() {
        assert_eq!(
            lit(&scalar("SHORTESTPATHLEN(<http://ex/a>, <http://ex/d>)")),
            "3"
        );
        assert_eq!(
            lit(&scalar("SHORTESTPATHLEN(<http://ex/a>, <http://ex/a>)")),
            "0"
        );
        // d→a is unreachable in the acyclic graph ⇒ -1 sentinel.
        assert_eq!(
            lit(&scalar("SHORTESTPATHLEN(<http://ex/d>, <http://ex/a>)")),
            "-1"
        );
    }

    #[test]
    fn khopreachable_respects_budget() {
        assert_eq!(
            lit(&scalar("KHOPREACHABLE(<http://ex/a>, <http://ex/d>, 3)")),
            "true"
        );
        assert_eq!(
            lit(&scalar("KHOPREACHABLE(<http://ex/a>, <http://ex/d>, 2)")),
            "false"
        );
        assert_eq!(
            lit(&scalar("KHOPREACHABLE(<http://ex/a>, <http://ex/a>, 0)")),
            "true"
        );
    }
}

/// Per-pair join-method selection (gStore `Join::judge`): nested-loop vs hash.
#[cfg(test)]
mod join_method_tests {
    use super::*;
    use crate::model::IdTriple;
    use crate::query::optimizer::{judge, JoinMethod};

    /// A tiny side (highly selective) picks nested-loop; two sizeable sides pick
    /// the general hash join.
    #[test]
    fn judge_picks_per_pair_method() {
        // One row joined against many ⇒ nested-loop (min·max ≤ |L|+|R|).
        assert_eq!(judge(1, 100), JoinMethod::NestedLoop);
        assert_eq!(judge(100, 1), JoinMethod::NestedLoop);
        assert_eq!(judge(2, 2), JoinMethod::NestedLoop);
        // Two sizeable sides ⇒ hash (quadratic cost dominates the linear hash).
        assert_eq!(judge(50, 50), JoinMethod::Hash);
        assert_eq!(judge(10, 1000), JoinMethod::Hash);
        // Empty side: either way the result is empty; nested-loop is fine.
        assert_eq!(judge(0, 1000), JoinMethod::NestedLoop);
    }

    fn b(vals: &[(usize, u32)], width: usize) -> Binding {
        let mut row = vec![None; width];
        for &(i, v) in vals {
            row[i] = Some(v);
        }
        row
    }

    /// Both physical methods produce byte-for-byte identical output on the same
    /// inputs — selection only changes cost, never the result.
    #[test]
    fn hash_and_nested_agree() {
        // width-3 bindings, join on shared variable index 0.
        let left: Vec<Binding> = (0..40u32).map(|x| b(&[(0, x), (1, x + 1000)], 3)).collect();
        let right: Vec<Binding> = (0..40u32)
            .filter(|x| x % 2 == 0)
            .map(|x| b(&[(0, x), (2, x + 5000)], 3))
            .collect();
        let shared = vec![0usize];
        let via_hash = join_sets_hash(&left, &right, &shared);
        let via_nested = join_sets_nested(&left, &right, &shared);
        assert_eq!(via_hash, via_nested);
        // And both equal what the dispatcher returns (whichever method it chose).
        let via_dispatch = join_sets(&left, &[0, 1], &right, &[0, 2]);
        assert_eq!(via_dispatch, via_hash);
        // Sanity: 20 even subjects matched ⇒ 20 joined rows, fully bound.
        assert_eq!(via_hash.len(), 20);
        for row in &via_hash {
            assert!(row[0].is_some() && row[1].is_some() && row[2].is_some());
        }
    }

    /// End-to-end query whose join binds a 1-row selective pattern to a larger
    /// one. After the per-pair `judge` wiring the engine still produces the
    /// correct answer (the method choice is invisible to results).
    #[test]
    fn query_join_with_tiny_selective_side() {
        use crate::parser::sparql::parse;
        let mut d = Dictionary::new();
        let knows = d.intern_predicate(&Term::iri("http://ex/knows").dict_key());
        let age = d.intern_predicate(&Term::iri("http://ex/age").dict_key());
        let alice = d.intern_term(&Term::iri("http://ex/alice"));
        let xsdint = "http://www.w3.org/2001/XMLSchema#integer";
        // alice knows exactly one person, p7; 50 people have ages.
        let mut people = Vec::new();
        for i in 0..50u32 {
            people.push(d.intern_term(&Term::iri(format!("http://ex/p{i}"))));
        }
        let mut triples = vec![IdTriple::new(alice, knows, people[7])];
        for (i, &pid) in people.iter().enumerate() {
            let a = d.intern_term(&Term::typed_literal(format!("{i}"), xsdint));
            triples.push(IdTriple::new(pid, age, a));
        }
        let mut s = TripleStore::new();
        s.bulk_load(triples);

        let q = parse(
            "SELECT ?f ?a WHERE { <http://ex/alice> <http://ex/knows> ?f . ?f <http://ex/age> ?a }",
        )
        .unwrap();
        let rs = match Evaluator::new(&d, &s).evaluate(&q).unwrap() {
            QueryResult::Select(rs) => rs,
            other => panic!("expected SELECT, got {other:?}"),
        };
        assert_eq!(rs.row_count(), 1);
        assert_eq!(rs.rows[0][0], Some("<http://ex/p7>".into()));
        assert_eq!(
            rs.rows[0][1],
            Some("\"7\"^^<http://www.w3.org/2001/XMLSchema#integer>".into())
        );
    }

    /// Drive the bushy binary-join executor (which routes through `join_sets`,
    /// hence `judge`) and confirm the result matches the left-deep pipeline on
    /// the same data — proving the method dispatch is wired into execution
    /// without changing results.
    #[test]
    fn bushy_execution_matches_left_deep() {
        use crate::parser::sparql::parse;
        // Two 2-stars joined by a bridge ⇒ the optimizer may pick a bushy tree,
        // executed via join_sets. Build entities so everything renders.
        let mut d = Dictionary::new();
        let ta = d.intern_predicate(&Term::iri("http://ex/ta").dict_key());
        let tb = d.intern_predicate(&Term::iri("http://ex/tb").dict_key());
        let tc = d.intern_predicate(&Term::iri("http://ex/tc").dict_key());
        let td = d.intern_predicate(&Term::iri("http://ex/td").dict_key());
        let bridge = d.intern_predicate(&Term::iri("http://ex/bridge").dict_key());
        let c900 = d.intern_term(&Term::iri("http://ex/c900"));
        let c901 = d.intern_term(&Term::iri("http://ex/c901"));
        let c902 = d.intern_term(&Term::iri("http://ex/c902"));
        let c903 = d.intern_term(&Term::iri("http://ex/c903"));
        let mut xs = Vec::new();
        for i in 0..30u32 {
            xs.push(d.intern_term(&Term::iri(format!("http://ex/x{i}"))));
        }
        let mut ys = Vec::new();
        for i in 0..30u32 {
            ys.push(d.intern_term(&Term::iri(format!("http://ex/y{i}"))));
        }
        let mut triples = Vec::new();
        for &x in &xs {
            triples.push(IdTriple::new(x, ta, c900));
            triples.push(IdTriple::new(x, tb, c901));
        }
        for &y in &ys {
            triples.push(IdTriple::new(y, tc, c902));
            triples.push(IdTriple::new(y, td, c903));
        }
        // one bridge edge x5 -> y9
        triples.push(IdTriple::new(xs[5], bridge, ys[9]));
        let mut s = TripleStore::new();
        s.bulk_load(triples);

        let q = parse(
            "SELECT ?x ?y WHERE {
                ?x <http://ex/ta> <http://ex/c900> .
                ?x <http://ex/tb> <http://ex/c901> .
                ?y <http://ex/tc> <http://ex/c902> .
                ?y <http://ex/td> <http://ex/c903> .
                ?x <http://ex/bridge> ?y .
             }",
        )
        .unwrap();
        let rs = match Evaluator::new(&d, &s).evaluate(&q).unwrap() {
            QueryResult::Select(rs) => rs,
            other => panic!("expected SELECT, got {other:?}"),
        };
        // Exactly the bridged pair survives all five patterns.
        assert_eq!(rs.row_count(), 1);
        assert_eq!(rs.rows[0][0], Some("<http://ex/x5>".into()));
        assert_eq!(rs.rows[0][1], Some("<http://ex/y9>".into()));
    }
}

/// User-defined scalar functions (gStore `pfnQuery`, in-process registry).
#[cfg(test)]
mod pfn_tests {
    use super::*;
    use crate::model::IdTriple;
    use crate::parser::sparql::parse;
    use crate::query::FunctionRegistry;

    /// alice salary 2500 ; bob salary 3000.
    fn fixture() -> (Dictionary, TripleStore) {
        let mut d = Dictionary::new();
        let alice = d.intern_term(&Term::iri("http://ex/alice"));
        let bob = d.intern_term(&Term::iri("http://ex/bob"));
        let salary = d.intern_predicate(&Term::iri("http://ex/salary").dict_key());
        let xsdint = "http://www.w3.org/2001/XMLSchema#integer";
        let s2500 = d.intern_term(&Term::typed_literal("2500", xsdint));
        let s3000 = d.intern_term(&Term::typed_literal("3000", xsdint));
        let mut s = TripleStore::new();
        s.bulk_load(vec![
            IdTriple::new(alice, salary, s2500),
            IdTriple::new(bob, salary, s3000),
        ]);
        (d, s)
    }

    /// Register `myDouble(?x)` and use it in a SELECT projection.
    #[test]
    fn custom_function_in_select() {
        let (d, s) = fixture();
        let q = parse(
            "SELECT ?x (myDouble(?sal) AS ?d) WHERE { ?x <http://ex/salary> ?sal } ORDER BY ?sal",
        )
        .unwrap();
        let eval = Evaluator::new(&d, &s).with_function("myDouble", |args| {
            Some(Value::Double(args.first()?.as_f64()? * 2.0))
        });
        let rs = match eval.evaluate(&q).unwrap() {
            QueryResult::Select(rs) => rs,
            other => panic!("expected SELECT, got {other:?}"),
        };
        assert_eq!(rs.row_count(), 2);
        // alice 2500 → 5000, bob 3000 → 6000 (rendered as xsd:double).
        assert!(rs.rows[0][1].as_ref().unwrap().contains("5000"));
        assert!(rs.rows[1][1].as_ref().unwrap().contains("6000"));
    }

    /// Use a custom predicate function inside a FILTER (case-insensitive name).
    #[test]
    fn custom_function_in_filter() {
        let (d, s) = fixture();
        let q = parse(
            "SELECT ?x WHERE { ?x <http://ex/salary> ?sal . FILTER(isHigh(?sal)) }",
        )
        .unwrap();
        let mut reg = FunctionRegistry::new();
        reg.register("isHigh", |args| {
            Some(Value::Bool(args.first()?.as_f64()? > 2800.0))
        });
        let rs = match Evaluator::new(&d, &s).with_functions(reg).evaluate(&q).unwrap() {
            QueryResult::Select(rs) => rs,
            other => panic!("expected SELECT, got {other:?}"),
        };
        // Only bob (3000) passes.
        assert_eq!(rs.row_count(), 1);
        assert_eq!(rs.rows[0][0], Some("<http://ex/bob>".into()));
    }

    /// An unregistered function name still errors (the solution is dropped),
    /// matching the pre-registry behaviour for unknown built-ins.
    #[test]
    fn unknown_function_excludes_solution() {
        let (d, s) = fixture();
        let q = parse(
            "SELECT ?x (noSuchFn(?sal) AS ?d) WHERE { ?x <http://ex/salary> ?sal }",
        )
        .unwrap();
        let rs = match Evaluator::new(&d, &s).evaluate(&q).unwrap() {
            QueryResult::Select(rs) => rs,
            other => panic!("expected SELECT, got {other:?}"),
        };
        // Rows still appear (the BGP matches) but ?d is unbound everywhere.
        assert_eq!(rs.row_count(), 2);
        assert!(rs.rows.iter().all(|r| r[1].is_none()));
    }

    /// Large conjunctive query (20 patterns) exercises the greedy+2-opt planner
    /// path (n > the exact-DP cap) and still returns the correct unique answer.
    #[test]
    fn large_bgp_20_patterns_correct() {
        use crate::parser::sparql::parse;
        let mut d = Dictionary::new();
        // 21 chain nodes n0..n20 and 20 predicates p0..p19.
        let mut nodes = Vec::new();
        for i in 0..=20u32 {
            nodes.push(d.intern_term(&Term::iri(format!("http://ex/n{i}"))));
        }
        let mut preds = Vec::new();
        for i in 0..20u32 {
            preds.push(d.intern_predicate(&Term::iri(format!("http://ex/p{i}")).dict_key()));
        }
        let mut triples = Vec::new();
        // the unique satisfying chain n0 -p0-> n1 -p1-> ... -p19-> n20
        for i in 0..20usize {
            triples.push(IdTriple::new(nodes[i], preds[i], nodes[i + 1]));
        }
        // distractors: dead-end edges under each predicate that cannot chain.
        for i in 0..20u32 {
            let dead = d.intern_term(&Term::iri(format!("http://ex/dead{i}")));
            for j in 0..5u32 {
                let src = d.intern_term(&Term::iri(format!("http://ex/junk{i}_{j}")));
                triples.push(IdTriple::new(src, preds[i as usize], dead));
            }
        }
        let mut s = TripleStore::new();
        s.bulk_load(triples);

        // Build the 20-pattern chain query.
        let mut where_clause = String::new();
        for i in 0..20 {
            where_clause.push_str(&format!("?a{i} <http://ex/p{i}> ?a{} . ", i + 1));
        }
        let q = parse(&format!("SELECT ?a0 ?a20 WHERE {{ {where_clause} }}")).unwrap();
        let rs = match Evaluator::new(&d, &s).evaluate(&q).unwrap() {
            QueryResult::Select(rs) => rs,
            other => panic!("expected SELECT, got {other:?}"),
        };
        assert_eq!(rs.row_count(), 1, "exactly one chain should match");
        assert_eq!(rs.rows[0][0], Some("<http://ex/n0>".into()));
        assert_eq!(rs.rows[0][1], Some("<http://ex/n20>".into()));
    }

    /// A registered name never shadows a real built-in (built-ins win).
    #[test]
    fn builtin_takes_precedence() {
        let (d, s) = fixture();
        let q = parse("SELECT (ABS(-5) AS ?r) WHERE { ?x <http://ex/salary> ?sal } LIMIT 1").unwrap();
        let eval = Evaluator::new(&d, &s).with_function("ABS", |_| Some(Value::Int(999)));
        let rs = match eval.evaluate(&q).unwrap() {
            QueryResult::Select(rs) => rs,
            other => panic!("expected SELECT, got {other:?}"),
        };
        // Real ABS(-5)=5, not the custom 999.
        assert!(rs.rows[0][0].as_ref().unwrap().contains('5'));
        assert!(!rs.rows[0][0].as_ref().unwrap().contains("999"));
    }
}
