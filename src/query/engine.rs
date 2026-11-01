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
//!
//! # Advanced graph functions (gStore 1.4)
//!
//! Upstream's advanced functions — `shortestPath(?x, <b>, true, {<p>})`,
//! `PR(true, {}, 10, alpha=0.85, maxIter=100, tol=0.001)`, `WCC({})`, … (the
//! parser's [`Expr::GraphFn`]) — are evaluated as upstream's
//! `GeneralEvaluation::getFinalResult` does, by the engine-independent library
//! [`crate::analytics::gfun`] over the per-predicate
//! [`CsrGraph`]:
//!
//! * **Where.** Only as the whole expression of a SELECT projection,
//!   `(f(…) AS ?v)`. Upstream also *parses* `simpleCycleBoolean`,
//!   `cycleBoolean`, `shortestPathLen` and `kHopReachable` inside FILTER but
//!   never evaluates them there (no case in `TempResult::doComp`); here any
//!   nested use is a query error that points to the legacy `rs:` function.
//! * **Arguments.** A variable vertex argument contributes the *distinct*
//!   values it is bound to across the WHERE solutions, in first-appearance
//!   order (rows where it is unbound are skipped; a variable bound only by
//!   `BIND` has no usable value at all, whatever it computes, because
//!   upstream keeps BIND results in string columns and skips them; another
//!   computed value that is not a stored term is skipped too; a stored
//!   literal is passed as an edge-less vertex, as upstream passes its literal
//!   id); a constant IRI is one vertex (unknown → upstream's `INVALID` id,
//!   [`GNode::MISSING`]). Two vertex arguments are evaluated over the
//!   cross product of their distinct values; a vertex variable without any
//!   usable value gives no entry (`{"paths":[]}`, `false` for
//!   `cycleBoolean`), as upstream's loop never runs. The `predSet` resolves
//!   against the dictionary (`{}` = every predicate, `!<p>` = every
//!   predicate but these, unknown IRIs dropped).
//! * **Row shape** (GE:1508-2050, GE:2546-3108). A lone projected function
//!   yields exactly one row computed over all solutions; next to other
//!   projections there is one row per solution and the function's value is
//!   on the *last* one only (WHERE order, before ORDER BY), the other rows
//!   leave it unbound; with GROUP BY it is computed once per group. ORDER BY
//!   variables that are not projected count as projections, because upstream
//!   appends them to its projection list. With no solutions — including
//!   `WHERE {}`, which upstream evaluates to an empty table — a lone function
//!   is evaluated once with its variables read as IRIs (so they are missing
//!   vertices; `kHopNeighbor` then names its list `"dst"`, as upstream's
//!   separate code path does) and anything else yields no row. DISTINCT,
//!   OFFSET and LIMIT apply to the rows as usual. Next to standard aggregates
//!   (`COUNT` …) without GROUP BY the query keeps SPARQL's one-row aggregate
//!   semantics (upstream would emit one mostly empty row per solution).
//! * **LIMIT pushdown.** When the WHERE clause is a single BGP (triple
//!   patterns only) with a LIMIT and no ORDER BY, upstream stops the BGP
//!   after LIMIT rows (`singleBGP` + the `limitK` strategy; OFFSET is not
//!   added), so the functions — and GROUP BY, aggregates and the row shape
//!   above — see only the first LIMIT solutions (in this engine's solution
//!   order; upstream's depends on its join order). OFFSET and LIMIT then
//!   apply to the rows as usual.
//! * **HAVING.** Upstream treats a projected function as an aggregate, so
//!   without GROUP BY the solutions form one group and a failing HAVING
//!   leaves every function value unbound (the rows stay).
//! * **Values.** The `{"paths":[…]}` JSON text is a plain literal, exactly as
//!   upstream emits it; `cycleBoolean` is an `xsd:boolean`;
//!   `confidenceDegree`'s JSON array (upstream's `pfn_type`, which replaces
//!   the whole upstream response) is a plain literal too.
//! * **CSR.** [`Database`](crate::Database) hands its cached CSR to the
//!   evaluator ([`Evaluator::with_csr_provider`]): it is built on the first
//!   graph-function query after a data change and shared by later queries.
//! * **Cancellation.** The library polls the evaluator's cancel flag.
//!
//!
//! # PFN calls
//!
//! `PFN.name({…})` — a user's personalized function ([`crate::pfn`]) — is
//! evaluated with upstream's projection shape (whole SELECT projection only;
//! over solutions one row per solution with the value on the last — also
//! when it is the lone projection, where a graph function gives one row —
//! per group with GROUP BY, once with its argument as written when there is
//! no solution). A failing call inside a sub-SELECT fails the query. Its
//! arguments follow upstream's `pfnQueryByVar` (see [`crate::pfn::call`]);
//! the value is a plain literal with the function's output (no solutions) or
//! the JSON array of its outputs. The function comes from the caller set
//! with [`Evaluator::with_pfn`] — a query reaches only its own user's
//! functions — and runs over the shared CSR under the cancel flag and the
//! graph-function output bound.
//!
//! # IRI function calls
//!
//! `<iri>(…)` / `pfx:name(…)` ([`Expr::IriCall`]) is, in order: a legacy
//! gstore-rust graph built-in in the predeclared `rs:` namespace
//! (`<urn:gstore-rust:fn:NAME>`, e.g. `rs:SSSPLEN(<a>, <b>)`, unchanged
//! semantics, usable anywhere); a SPARQL 1.1 XSD constructor cast
//! (`xsd:string`, `xsd:integer`, `xsd:decimal`, `xsd:double`, `xsd:float`,
//! `xsd:boolean`, `xsd:dateTime`, and `xsd:date`; a cast the §17.5 table
//! forbids or an invalid lexical form is an expression error, i.e. unbound);
//! or a user function registered on the [`Database`](crate::Database) under
//! the full IRI or its local name. Any other IRI call is a query error.
//!
//! # SPARQL 1.1 details
//!
//! * `IN` / `NOT IN` follow §17.4.1.9-10: `=` on each member, where two
//!   terms that are not both literals are simply unequal (RDFterm-equal) and
//!   two incomparable literals are a type error; an error only counts when
//!   no member is equal (see `eval_in`).
//! * A trailing `VALUES` block ([`SelectQuery::values`]) joins the WHERE
//!   solutions of a query without grouping; after GROUP BY / aggregation it
//!   joins the aggregated rows on their projected names (a VALUES variable
//!   that is not a result column only multiplies the rows), before ORDER BY.
//! * A hidden blank-node variable ([`ANON_VAR_PREFIX`], from `[ … ]` /
//!   `( … )`) never shows in `SELECT *` / `DESCRIBE *`; in a CONSTRUCT or
//!   INSERT template, one the solution does not bind is a fresh blank node per
//!   solution.
//! * gStore's `CONTAINALL` / `CONTAINANY` test `xsd:list` literals (see
//!   `eval_contain`).
//!
//! # Aggregate queries
//!
//! A SELECT is an aggregate query when it has `GROUP BY`, an aggregate in a
//! SELECT expression, `HAVING`, or an aggregate in `ORDER BY` (the last two
//! form one implicit group). As SPARQL 1.1 §18.2.4.1 specifies, every
//! aggregate in the SELECT expressions, `HAVING` and `ORDER BY` is computed
//! once per group into a hidden column of the group's row, and the
//! expressions are then evaluated over that row like any other expression —
//! so built-ins, casts, arithmetic, `IF` and `COALESCE` wrap aggregates
//! freely (`ROUND(AVG(?x))`, `(SUM(?a)/COUNT(?a) AS ?avg)`,
//! `HAVING (STR(COUNT(*)) = "2")`, `ORDER BY DESC(SUM(?x))`). SELECT
//! expressions are evaluated left to right, so a later one may use an earlier
//! alias, and `ORDER BY` may use any alias. `GROUP BY ?v` groups by the term
//! `?v` is bound to (two different terms with equal values, such as
//! `"1"^^xsd:integer` and `"01"^^xsd:integer`, are different groups). A
//! variable that is neither grouped nor aggregated reads its value in the
//! group's first solution (SPARQL would reject the query).
//!
//! # Arithmetic
//!
//! Numbers follow XPath type promotion (see [`Value`]). Dividing two integers
//! gives an `xsd:decimal` by default (`7/2` = `3.5`); upstream gStore's
//! truncating integer division (`7/2` = `3`, also inside `AVG`) is an opt-in
//! mode, [`Evaluator::with_integer_division`] /
//! [`Database::set_integer_division`](crate::Database::set_integer_division).
//!
//! # LIMIT, ORDER BY … LIMIT, ASK and EXISTS
//!
//! Step 5 above does not always materialize everything (module `topk`,
//! source `src/query/topk.rs`): a SELECT or CONSTRUCT with `LIMIT` and no
//! `ORDER BY`, `ASK` and `EXISTS` stream their solutions and stop early;
//! `ORDER BY … LIMIT k` keeps the best `OFFSET + k` solutions in a bounded
//! heap; and `ORDER BY` of an integer linear expression over a tree-shaped
//! BGP enumerates matches in key order (upstream's DP-B). Each returns the
//! exact rows, in the exact order, of the full evaluation — ties keep their
//! WHERE order — and [`TopKOptions`] switches them off
//! ([`Evaluator::with_topk_options`] /
//! [`Database::set_topk_options`](crate::Database::set_topk_options));
//! [`EvalStats`] counts the work.
//!
//! # SPARQL Update templates
//!
//! [`Evaluator::eval_update_modify`] instantiates `DELETE`/`INSERT` quad
//! templates (`GRAPH <g> { … }` / `GRAPH ?g { … }` blocks included) per WHERE
//! solution; the [`Database`](crate::Database) chooses the WHERE dataset
//! (`WITH`, `USING`, `USING NAMED`) and applies the quads.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use regex::Regex;

use crate::analytics::csr::CsrGraph;
use crate::analytics::gfun::{self, GCall, GFnKind, GFunCtx, GFunValue, GNode};
use crate::analytics::GraphView;
use crate::dict::Dictionary;
use crate::error::{GStoreError, Result};
use crate::kvstore::StringIndex;
use crate::model::id::PredId;
use crate::model::{Term, Triple};
use crate::parser::ntriples::parse_term;
use crate::parser::sparql::ast::*;
use crate::parser::sparql::{builtins, rs_function_name, RS_NAMESPACE};
use crate::signature::{EdgeDir, Signature, VsTree};
use crate::store::{TripleSource, TripleStore};

use super::candidates::{self, Candidates};
use super::functions::FunctionRegistry;
use super::hash;
use super::optimizer::{self, ExecPlan, JoinMethod, JoinTree};
use super::results::{QueryResult, ResultSet};
use super::value::{
    format_xsd_datetime_utc, is_decimal_lexical, is_integer_lexical, order_key,
    parse_datetime_parts, parse_xsd_double, to_f32_precision, xsd_float_lexical, DateTimeParts,
    IntegerDivision, NumericFn, Value, XSD_FLOAT,
};

/// The term a dictionary key names ([`Term::from_dict_key`]: verbatim, so a
/// term a lenient reader let in — an IRI holding `>` — is never lost on its
/// way out of the store), else the N-Triples reading of `s`.
fn key_term(s: &str) -> Option<Term> {
    Term::from_dict_key(s).or_else(|| parse_term(s).ok())
}

// Top-K, bounded-heap ORDER BY and early termination for LIMIT / ASK /
// EXISTS (docs/GAP_PLAN.md §3.4). A child module so it can drive the private
// BGP/join machinery below.
#[path = "topk.rs"]
mod topk;
use topk::PreparedBgp;
pub use topk::{DpbMode, EvalStats, TopKOptions};

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
    /// Whether each variable's ids are *predicate*-dictionary ids (→ resolve
    /// them via the predicate dictionary rather than the entity/literal one):
    /// true for a variable that appears only in predicate position.
    is_pred: Vec<bool>,
    /// Mixed variables: a variable in predicate position that is also bound
    /// some other way (subject/object, BIND, VALUES, sub-SELECT, GRAPH, …).
    /// Its own slot holds entity/literal-space ids (`is_pred` false) and its
    /// predicate-position occurrences bind the hidden predicate-space slot
    /// it maps to here; [`Evaluator::eval_bgp_constrained`] keeps the two
    /// in step (same RDF term), so joins never compare a predicate id with
    /// an entity id.
    pred_alias: HashMap<usize, usize>,
}

/// Name prefix of the hidden predicate-space slot of a mixed variable (see
/// [`VarLayout::pred_alias`]); a SPARQL variable name cannot contain NUL.
const PRED_ALIAS_PREFIX: &str = "\u{0}pred:";

/// A variable's exact candidate set is refined by probing the in-memory
/// VS-tree entry by entry, instead of walking the tree, when it holds at most
/// `max(PROBE_MIN_CANDIDATES, indexed entities / 32)` ids (see
/// `refine_candidates_with_vstree`).
const PROBE_MIN_CANDIDATES: usize = 1024;

impl VarLayout {
    fn build(pattern: &GraphPattern) -> VarLayout {
        // All variables the pattern can bind (BGP, BIND, VALUES, sub-SELECT, …).
        let mut names = Vec::new();
        pattern.collect_vars(&mut names);
        let mut index: HashMap<String, usize> = names
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
        // A predicate variable bound any other way too is mixed: it keeps
        // entity-space ids and gets a hidden predicate-space slot.
        let mut node_bound = HashSet::new();
        node_bound_vars(pattern, &mut node_bound);
        let mut pred_alias = HashMap::new();
        for i in 0..names.len() {
            if is_pred[i] && node_bound.contains(&names[i]) {
                is_pred[i] = false;
                let hidden = format!("{PRED_ALIAS_PREFIX}{}", names[i]);
                let h = names.len();
                index.insert(hidden.clone(), h);
                names.push(hidden);
                is_pred.push(true);
                pred_alias.insert(i, h);
            }
        }
        VarLayout {
            index,
            names,
            is_pred,
            pred_alias,
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
            pred_alias: HashMap::new(),
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
        // The synthetic range is `[SYNTH_BASE, u32::MAX)`. Once it is full (~95M
        // distinct computed terms in one query) further terms get the
        // unresolvable sentinel `u32::MAX` — they read back as unbound — rather
        // than overflowing the id arithmetic.
        let Some(id) = u32::try_from(self.terms.len())
            .ok()
            .and_then(|n| SYNTH_BASE.checked_add(n))
            .filter(|&id| id != u32::MAX)
        else {
            return u32::MAX;
        };
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
    /// Optional cooperative-cancellation flag (gStore's task-cancel). When set to
    /// `true`, a running read aborts at the next pattern/phase boundary with
    /// [`GStoreError::Cancelled`] (see [`with_cancel`](Self::with_cancel)).
    cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Memoized whole-graph analytics results (WCC/Louvain/betweenness/…), so a
    /// graph built-in used in a per-row `FILTER`/projection computes its
    /// full-graph pass **once per query** instead of once per row. Keyed only by
    /// algorithm (the graph is fixed for the query's lifetime); PPR is keyed by
    /// source node. Shared with `GRAPH` sub-evaluators is NOT desired (they run
    /// over a different store), so each evaluator gets its own.
    algo_cache: std::cell::RefCell<AlgoCache>,
    /// Lazily-built substring/prefix index over the (in-memory) dictionary, used
    /// to prune candidate sets for mandatory-substring `FILTER`s
    /// (`CONTAINS`/`STRSTARTS`/`STRENDS`/plain `REGEX`). Built at most once per
    /// query, the first time such a filter is seen. Stays `None` (disabled) for a
    /// disk-backed dictionary (no in-memory strings) or a dictionary too small
    /// for indexing to beat a scan; [`string_index_tried`](Self::string_index)
    /// guards against rebuilding.
    string_index: std::cell::RefCell<Option<Rc<StringIndex>>>,
    string_index_tried: std::cell::Cell<bool>,
    /// Whether `SERVICE` may contact remote endpoints (see
    /// [`with_service_allowed`](Self::with_service_allowed)).
    allow_service: bool,
    /// Set when a `SERVICE` was refused; like a cancel, it cuts evaluation
    /// short and is reported as an error. Shared with `GRAPH` sub-evaluators.
    service_denied: Rc<std::cell::Cell<bool>>,
    /// The first error of a sub-`SELECT` (a failing `PFN` or graph-function
    /// call, …): a pattern has no way to return one, so — like a refused
    /// `SERVICE` — it cuts evaluation short and is reported as the query's
    /// error instead of reading as "no solution". Shared with `GRAPH`
    /// sub-evaluators.
    failed: Rc<std::cell::RefCell<Option<GStoreError>>>,
    /// Where gStore's advanced graph functions get their per-predicate CSR
    /// ([`with_csr_provider`](Self::with_csr_provider)): the
    /// [`Database`](crate::Database)'s cached [`Database::csr`](crate::Database::csr),
    /// shared across queries. Without one the evaluator builds a CSR of its
    /// own store on first use (once per query).
    csr_provider: Option<&'a CsrProvider<'a>>,
    /// The CSR obtained for this query (from the provider or built), kept so
    /// several graph functions / groups in one query share it.
    gfun_csr: std::cell::RefCell<Option<Arc<CsrGraph>>>,
    /// How `/` divides two integers ([`with_integer_division`](Self::with_integer_division)).
    integer_division: IntegerDivision,
    /// Which LIMIT / ORDER BY / ASK optimizations may run
    /// ([`with_topk_options`](Self::with_topk_options); all on by default).
    topk: TopKOptions,
    /// Work counters ([`stats`](Self::stats)), shared with `GRAPH`
    /// sub-evaluators so their work counts too.
    stats: Rc<std::cell::Cell<EvalStats>>,
    /// The most bytes of JSON one graph-function call may produce
    /// ([`with_gfun_max_output_bytes`](Self::with_gfun_max_output_bytes)).
    gfun_max_output: usize,
    /// Who calls `PFN.name(…)` functions, from which registry
    /// ([`with_pfn`](Self::with_pfn)); `None` = PFN calls fail.
    pfn: Option<crate::pfn::PfnCaller>,
}

/// A source of the per-predicate CSR the advanced graph functions run on
/// (see [`Evaluator::with_csr_provider`]).
pub type CsrProvider<'a> = dyn Fn() -> Result<Arc<CsrGraph>> + 'a;

/// Per-query memoization of whole-graph analytics results (see
/// [`Evaluator::algo_cache`]). All maps are keyed by dense node id
/// ([`EntityLiteralId`](crate::model::id::EntityLiteralId) = `u32`).
#[derive(Default)]
struct AlgoCache {
    wcc: Option<Rc<HashMap<u32, u32>>>,
    louvain: Option<Rc<HashMap<u32, usize>>>,
    labelprop: Option<Rc<HashMap<u32, usize>>>,
    betweenness: Option<Rc<HashMap<u32, f64>>>,
    closeness: Option<Rc<HashMap<u32, f64>>>,
    ppr: HashMap<u32, Rc<HashMap<u32, f64>>>,
    degree_assortativity: Option<f64>,
    diameter: Option<u32>,
    global_clustering: Option<f64>,
    triangle_count: Option<u64>,
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
            cancel: None,
            algo_cache: std::cell::RefCell::new(AlgoCache::default()),
            string_index: std::cell::RefCell::new(None),
            string_index_tried: std::cell::Cell::new(false),
            allow_service: true,
            service_denied: Rc::new(std::cell::Cell::new(false)),
            failed: Rc::default(),
            csr_provider: None,
            gfun_csr: std::cell::RefCell::new(None),
            integer_division: IntegerDivision::default(),
            topk: TopKOptions::default(),
            stats: Rc::default(),
            gfun_max_output: crate::analytics::gfun::paths::MAX_OUTPUT_BYTES,
            pfn: None,
        }
    }

    /// Build an evaluator that uses `vstree` to prune entity-variable bindings.
    pub fn with_vstree(dict: &'a Dictionary, store: &'a S, vstree: &'a VsTree) -> Evaluator<'a, S> {
        Evaluator {
            dict,
            store,
            vstree: Some(vstree),
            named: None,
            extras: Rc::new(std::cell::RefCell::new(Extras::default())),
            plan_cache: std::cell::RefCell::new(HashMap::new()),
            graph_cache: std::cell::RefCell::new(None),
            functions: Rc::new(FunctionRegistry::new()),
            cancel: None,
            algo_cache: std::cell::RefCell::new(AlgoCache::default()),
            string_index: std::cell::RefCell::new(None),
            string_index_tried: std::cell::Cell::new(false),
            allow_service: true,
            service_denied: Rc::new(std::cell::Cell::new(false)),
            failed: Rc::default(),
            csr_provider: None,
            gfun_csr: std::cell::RefCell::new(None),
            integer_division: IntegerDivision::default(),
            topk: TopKOptions::default(),
            stats: Rc::default(),
            gfun_max_output: crate::analytics::gfun::paths::MAX_OUTPUT_BYTES,
            pfn: None,
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

    /// Attach a cooperative-cancellation flag (gStore's task cancel). Setting the
    /// flag to `true` from another thread aborts an in-progress read at the next
    /// pattern/phase boundary with [`GStoreError::Cancelled`]. Checked at query
    /// entry, between BGP join patterns, and after pattern evaluation — so a large
    /// multi-pattern join is cancellable, though a single unbounded scan is not
    /// preempted mid-scan.
    pub fn with_cancel(
        mut self,
        flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Evaluator<'a, S> {
        self.cancel = Some(flag);
        self
    }

    /// Allow or forbid federated `SERVICE` patterns (allowed by default). A
    /// server that must not make outbound requests on a client's behalf
    /// forbids them: a query using `SERVICE` then fails with an error — no
    /// connection is attempted, `SILENT` or not. [`Database`](crate::Database)
    /// sets this from its [`LoadPolicy`](crate::db::LoadPolicy)'s
    /// `allow_remote`.
    pub fn with_service_allowed(mut self, allowed: bool) -> Evaluator<'a, S> {
        self.allow_service = allowed;
        self
    }

    /// Supply the per-predicate CSR that gStore's advanced graph functions
    /// (`shortestPath(…)`, `PR(…)`, …) run on. [`Database`](crate::Database)
    /// passes its cached [`Database::csr`](crate::Database::csr), so the CSR
    /// is built once per data version and shared across queries; the provider
    /// is only called when a query actually projects a graph function.
    /// Without a provider the evaluator builds a CSR of its own store the
    /// first time a query needs one.
    pub fn with_csr_provider(mut self, provider: &'a CsrProvider<'a>) -> Evaluator<'a, S> {
        self.csr_provider = Some(provider);
        self
    }

    /// Choose how `/` divides two integers: SPARQL 1.1's decimal quotient
    /// ([`IntegerDivision::Decimal`], the default: `7/2` = `3.5`) or upstream
    /// gStore's truncation ([`IntegerDivision::Truncate`]: `7/2` = `3`, and
    /// `AVG` over integers truncates too). See [`Value::arith_with`].
    pub fn with_integer_division(mut self, mode: IntegerDivision) -> Evaluator<'a, S> {
        self.integer_division = mode;
        self
    }

    /// Bound the JSON one call of a gStore advanced graph function may
    /// produce (default
    /// [`paths::MAX_OUTPUT_BYTES`](crate::analytics::gfun::paths::MAX_OUTPUT_BYTES));
    /// a call that would exceed it fails the query (see
    /// [`GFunCtx::with_max_output_bytes`]).
    pub fn with_gfun_max_output_bytes(mut self, bytes: usize) -> Evaluator<'a, S> {
        self.gfun_max_output = bytes;
        self
    }

    /// Let `PFN.name({…})` projections call `caller`'s functions (gStore's
    /// personalized functions, see [`crate::pfn`]). They run over this
    /// query's CSR (see [`with_csr_provider`](Self::with_csr_provider)),
    /// under the caller's limits, this evaluator's cancel flag and
    /// [`with_gfun_max_output_bytes`](Self::with_gfun_max_output_bytes)
    /// bound. Without a caller a PFN call is a query error.
    pub fn with_pfn(mut self, caller: crate::pfn::PfnCaller) -> Evaluator<'a, S> {
        self.pfn = Some(caller);
        self
    }

    /// The CSR for the advanced graph functions of this query (see
    /// [`with_csr_provider`](Self::with_csr_provider)).
    fn gfun_csr(&self) -> Result<Arc<CsrGraph>> {
        if let Some(csr) = self.gfun_csr.borrow().as_ref() {
            return Ok(Arc::clone(csr));
        }
        let csr = match self.csr_provider {
            Some(provider) => provider()?,
            None => Arc::new(CsrGraph::build(self.store)?),
        };
        *self.gfun_csr.borrow_mut() = Some(Arc::clone(&csr));
        Ok(csr)
    }

    /// Whether evaluation must stop early: cancellation was requested, or a
    /// refused `SERVICE` or a failed sub-`SELECT` already doomed the query.
    fn cancelled(&self) -> bool {
        self.service_denied.get()
            || self.failed.borrow().is_some()
            || self
                .cancel
                .as_ref()
                .is_some_and(|c| c.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// `Err` if evaluation was cut short — [`GStoreError::Cancelled`] when
    /// cancellation was requested, a query error when a `SERVICE` was refused,
    /// a sub-`SELECT`'s error when one failed — else `Ok(())`.
    pub(crate) fn check_cancel(&self) -> Result<()> {
        if self.service_denied.get() {
            Err(GStoreError::Query(
                "SERVICE is disabled: this server does not contact remote endpoints on a \
                 client's behalf (its load policy does not allow remote sources)"
                    .into(),
            ))
        } else if let Some(e) = self.failed.borrow().as_ref() {
            Err(e.duplicate())
        } else if self.cancelled() {
            Err(GStoreError::Cancelled)
        } else {
            Ok(())
        }
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
        self.check_cancel()?;
        self.check_calls(query)?;
        let result = match query {
            Query::Select(s) => QueryResult::Select(self.eval_select(s)?),
            Query::Ask(a) => {
                let layout = VarLayout::build(&a.pattern);
                // Stops at the first solution (see `topk`).
                QueryResult::Ask(self.pattern_has_solution(&a.pattern, &layout))
            }
            Query::Construct(c) => QueryResult::Construct(self.eval_construct(c)?),
            Query::Describe(d) => QueryResult::Construct(self.eval_describe(d)?),
            Query::Update(_) => {
                return Err(GStoreError::Query(
                    "updates must be applied through Database, not the read evaluator".into(),
                ))
            }
        };
        // A cancel (or a refused `SERVICE`) cuts every later pattern short to
        // "no solution" — also one met after the WHERE clause, in a
        // projected / ORDER BY / HAVING `EXISTS` — so a result computed while
        // it was up may be wrong. Report it instead of answering (a cached
        // wrong answer would outlive the request). The flag only ever goes
        // up: if it is down here, it was down throughout.
        self.check_cancel()?;
        Ok(result)
    }

    /// Evaluate CONSTRUCT: instantiate the template once per solution, skipping
    /// any template triple with an unbound variable. Returns de-duplicated triples.
    fn eval_construct(&self, c: &ConstructQuery) -> Result<Vec<Triple>> {
        let layout = VarLayout::build(&c.pattern);
        // With a LIMIT, only the first (or, with ORDER BY, the best)
        // OFFSET+LIMIT solutions are needed (see `topk`): already in order.
        let (mut solutions, sorted) = match self.construct_fast_path(c, &layout)? {
            Some(prefix) => (prefix, true),
            None => (self.eval_pattern(&c.pattern, &layout), false),
        };
        // A mid-run cancel short-circuits eval_pattern to empty; surface it as an
        // error rather than a bogus empty CONSTRUCT (which would also poison the
        // query cache). Mirrors eval_select_solutions / the ASK path.
        self.check_cancel()?;
        if !c.order_by.is_empty() && !sorted {
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
            let mut fresh = HashMap::new();
            for tp in &c.template {
                // A triple with a literal subject or a non-IRI predicate is
                // not RDF and is left out (§16.2).
                if let Some(t) = self
                    .instantiate_template(tp, b, &layout, &mut fresh)
                    .filter(|t| !t.subject.is_literal() && matches!(t.predicate, Term::Iri(_)))
                {
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
                self.check_cancel()?; // convert a mid-run cancel into Cancelled, not an empty describe
                                      // Only describe resource-valued columns: predicate-typed columns
                                      // hold predicate ids (a different id space), and synthetic ids are
                                      // computed values, not stored resources.
                let want: Vec<usize> = if d.all {
                    (0..layout.len())
                        .filter(|&i| !layout.is_pred[i] && !is_anon_var(&layout.names[i]))
                        .collect()
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
                .and_then(|s| key_term(&s))
            else {
                continue;
            };
            for (p, o) in self.store.po_by_s(id) {
                let (Some(pred), Some(obj)) = (
                    self.resolve_string(Some(p), true)
                        .and_then(|s| key_term(&s)),
                    self.resolve_string(Some(o), false)
                        .and_then(|s| key_term(&s)),
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

    /// Compute the ground quads a `DELETE … INSERT … WHERE` modify would
    /// remove and add: evaluate the WHERE pattern over this evaluator's
    /// dataset (its store is the default graph, [`with_named`](Self::with_named)
    /// the named graphs), then instantiate each template against every
    /// solution, de-duplicated. Each quad comes with its target graph IRI —
    /// the template's `GRAPH <iri>`, or the IRI `GRAPH ?g` is bound to — or
    /// `None` for a template outside any `GRAPH` block (the caller's default
    /// graph, i.e. the `WITH` graph if there is one). A template quad is
    /// skipped for a solution that leaves one of its variables unbound or
    /// would make an illegal triple (a literal or blank-node predicate, a
    /// literal subject, a graph name that is not an IRI) — SPARQL 1.1 Update
    /// §3.1.3. The caller (Database) applies deletes before inserts.
    /// Read-only — does not mutate the store. Fails, like
    /// [`Evaluator::evaluate`], when the WHERE pattern calls an advanced
    /// graph function or `PFN` that the engine cannot evaluate yet.
    #[allow(clippy::type_complexity)]
    pub fn eval_update_modify(
        &self,
        delete: &[QuadPattern],
        insert: &[QuadPattern],
        pattern: &GraphPattern,
    ) -> Result<(Vec<(Option<String>, Triple)>, Vec<(Option<String>, Triple)>)> {
        self.check_pattern_calls(pattern)?;
        let layout = VarLayout::build(pattern);
        let sols = self.eval_pattern(pattern, &layout);
        self.check_cancel()?;
        let mut dels = Vec::new();
        let mut ins = Vec::new();
        let mut seen_d = HashSet::new();
        let mut seen_i = HashSet::new();
        for b in &sols {
            // One map per solution: a `[ … ]` / `( … )` node of the
            // templates is the same fresh blank node across this solution's
            // quads, and a new one for the next solution.
            let mut fresh = HashMap::new();
            for (templates, out, seen) in [
                (delete, &mut dels, &mut seen_d),
                (insert, &mut ins, &mut seen_i),
            ] {
                for q in templates {
                    let Some(quad) = self.instantiate_quad(q, b, &layout, &mut fresh) else {
                        continue;
                    };
                    if seen.insert((quad.0.clone(), quad.1.to_string())) {
                        out.push(quad);
                    }
                }
            }
        }
        Ok((dels, ins))
    }

    /// Fill a quad template from a solution (see
    /// [`eval_update_modify`](Self::eval_update_modify)); `None` if a
    /// variable is unbound or the result is not a legal quad. `fresh` is as
    /// for [`instantiate_template`](Self::instantiate_template).
    fn instantiate_quad(
        &self,
        q: &QuadPattern,
        b: &Binding,
        layout: &VarLayout,
        fresh: &mut HashMap<String, Term>,
    ) -> Option<(Option<String>, Triple)> {
        let graph = match &q.graph {
            None => None,
            Some(GraphTerm::Iri(iri)) => Some(iri.clone()),
            Some(GraphTerm::Var(v)) => match self.binding_term(v, b, layout)? {
                Term::Iri(iri) => Some(iri),
                _ => return None,
            },
        };
        let t = self.instantiate_template(&q.triple, b, layout, fresh)?;
        if t.subject.is_literal() || !matches!(t.predicate, Term::Iri(_)) {
            return None;
        }
        Some((graph, t))
    }

    /// The term variable `v` is bound to in `b`, resolved in the variable's
    /// own id space (a predicate-position variable holds predicate ids).
    fn binding_term(&self, v: &str, b: &Binding, layout: &VarLayout) -> Option<Term> {
        let idx = *layout.index.get(v)?;
        let s = self.resolve_string(b[idx], layout.is_pred[idx])?;
        key_term(&s)
    }

    /// Fill a template triple from a solution; `None` if any position is
    /// unbound. A hidden blank-node variable (`[ … ]` / `( … )` in the
    /// template, see [`ANON_VAR_PREFIX`]) the solution does not bind becomes
    /// a fresh blank node, the same one for every template triple of this
    /// solution: `fresh` holds them, and the caller starts a new map per
    /// solution.
    fn instantiate_template(
        &self,
        tp: &TriplePattern,
        b: &Binding,
        layout: &VarLayout,
        fresh: &mut HashMap<String, Term>,
    ) -> Option<Triple> {
        let mut resolve = |pt: &PatternTerm| -> Option<Term> {
            match pt {
                PatternTerm::Term(t) => Some(t.clone()),
                PatternTerm::Var(v) => match self.binding_term(v, b, layout) {
                    None if is_anon_var(v) => Some(
                        fresh
                            .entry(v.clone())
                            .or_insert_with(|| Term::Blank(fresh_bnode_label()))
                            .clone(),
                    ),
                    other => other,
                },
            }
        };
        Some(Triple::new(
            resolve(&tp.subject)?,
            resolve(&tp.predicate)?,
            resolve(&tp.object)?,
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
        // gStore's advanced graph functions, `(shortestPath(…) AS ?v)`: each is
        // computed once over the distinct bindings of its vertex arguments
        // across all solutions (per group with GROUP BY) and placed with
        // upstream's row shape — see the module docs, "Advanced graph
        // functions".
        let gfns = graph_fn_items(q)?;
        // An aggregate query: GROUP BY, an aggregate in a SELECT expression,
        // or HAVING / an aggregate in ORDER BY (one implicit group). Next to
        // a projected graph function, HAVING keeps upstream's row shape
        // instead (see the module docs).
        let has_agg = !q.group_by.is_empty()
            || projection_has_aggregate(&q.projection)
            || (gfns.is_empty()
                && (!q.having.is_empty()
                    || q.order_by.iter().any(|o| expr_has_aggregate(&o.expr))));
        // A trailing `VALUES` block joins the solutions before the SELECT
        // expressions, ORDER BY and projection (§18.2.4.3): without grouping
        // that is a join with the WHERE pattern; after grouping it joins the
        // group rows after HAVING (in `eval_aggregation`).
        let with_values;
        let pattern = match &q.values {
            Some((vars, rows)) if !has_agg => {
                let values = GraphPattern::Values(vars.clone(), rows.clone());
                with_values = match &q.pattern {
                    GraphPattern::Empty => values,
                    p => GraphPattern::Join(Box::new(p.clone()), Box::new(values)),
                };
                &with_values
            }
            _ => &q.pattern,
        };
        let mut layout = VarLayout::build(pattern);
        let result_vars = q.result_vars();
        // A result column is predicate-typed only if it is a plain pattern
        // variable that occurs in predicate position.
        let is_pred: Vec<bool> = result_vars
            .iter()
            .map(|v| layout.index.get(v).is_some_and(|&i| layout.is_pred[i]))
            .collect();

        // LIMIT without ORDER BY streams and stops early; ORDER BY + LIMIT
        // keeps only the best OFFSET+LIMIT solutions (bounded heap, DP-B).
        // Both return a long enough prefix of the rows the full evaluation
        // below would produce (see `topk`).
        if gfns.is_empty() && !has_agg {
            if let Some(rows) = self.select_fast_path(q, pattern, &layout, &result_vars)? {
                return Ok((result_vars, is_pred, apply_row_modifiers(q, rows)));
            }
        }

        // Upstream's LIMIT pushdown: a WHERE clause that is one BGP, with a
        // LIMIT and no ORDER BY, stops its BGP after LIMIT rows (OFFSET is not
        // added), so the functions only see those rows.
        let gfn_limit = match q.limit {
            Some(limit)
                if !gfns.is_empty() && q.order_by.is_empty() && pattern_is_single_bgp(pattern) =>
            {
                Some(limit)
            }
            _ => None,
        };
        let mut solutions = match gfn_limit {
            Some(limit) => self.first_solutions(pattern, &layout, limit),
            None => self.eval_pattern(pattern, &layout),
        };
        // A cancelled join short-circuits to empty bindings; surface it as an
        // error rather than a spurious empty result set.
        self.check_cancel()?;
        if let Some(limit) = gfn_limit {
            solutions.truncate(limit);
        }
        let bind_only = if gfns.is_empty() {
            HashSet::new()
        } else {
            bind_only_vars(pattern)
        };
        let only_gfn = !gfns.is_empty() && upstream_projection_len(q) == 1;
        // Upstream reserves one row for a lone graph function but not for a
        // lone PFN call (GE:2043-2050 excludes `PFN_type`): over solutions
        // that one keeps a row per solution, its value on the last, as next
        // to other projections (GE:2526-2530).
        let one_row = only_gfn && !matches!(gfns[0].call, ProjCall::Pfn { .. });
        let rows =
            if !gfns.is_empty() && (solutions.is_empty() || pattern_is_trivially_empty(pattern)) {
                // Upstream's "result0_size == 0" (GE:1508-2033): a lone graph
                // function is evaluated once with its arguments read as constants
                // (a variable is looked up as an IRI and is missing); next to
                // other projections there is no row at all.
                if only_gfn {
                    vec![vec![self.eval_proj_call(
                        &gfns[0].call,
                        &[],
                        &layout,
                        &bind_only,
                        true,
                    )?]]
                } else {
                    Vec::new()
                }
            } else if one_row && !has_agg {
                // A lone graph function over the solutions: exactly one row
                // (upstream reserves `resultSz = 1`, GE:2041-2050). The solutions
                // form one group: a failing HAVING leaves the value unset
                // (GE:2071-2075).
                let all: Vec<&Binding> = solutions.iter().collect();
                let value = if self.having_holds(q, &all, &layout) {
                    self.eval_proj_call(&gfns[0].call, &all, &layout, &bind_only, false)?
                } else {
                    None
                };
                vec![vec![value]]
            } else if has_agg {
                // Grouped, filtered by HAVING, joined with a trailing VALUES and
                // ordered by ORDER BY there.
                self.eval_aggregation(&solutions, &layout, &bind_only, q)?
            } else {
                // Graph functions next to other projections: one row per
                // solution, each function's value on the *last* solution only
                // (upstream writes it to `result.back()`, GE:3100-3108; "last" is
                // in WHERE order, before ORDER BY), or on none when HAVING fails
                // over the solutions as one group. The value becomes a column
                // under its alias so ORDER BY can refer to it.
                if !gfns.is_empty() {
                    let all: Vec<&Binding> = solutions.iter().collect();
                    let keep = self.having_holds(q, &all, &layout);
                    let mut values = Vec::with_capacity(gfns.len());
                    for g in &gfns {
                        values.push(if keep {
                            self.eval_proj_call(&g.call, &all, &layout, &bind_only, false)?
                        } else {
                            None
                        });
                    }
                    for (g, v) in gfns.iter().zip(values) {
                        if layout.index.contains_key(&g.alias) {
                            return Err(GStoreError::Query(format!(
                                "?{} is already bound by the WHERE clause; ({}(…) AS ?v) needs a \
                             fresh variable",
                                g.alias,
                                g.call.label()
                            )));
                        }
                        layout.index.insert(g.alias.clone(), layout.names.len());
                        layout.names.push(g.alias.clone());
                        layout.is_pred.push(false);
                        let last = solutions.len().saturating_sub(1);
                        for (i, b) in solutions.iter_mut().enumerate() {
                            b.push(if i == last { v } else { None });
                        }
                    }
                }
                // SPARQL evaluates ORDER BY over the *pre-projection* solution
                // sequence, so it may sort by a pattern variable that is not selected
                // (e.g. `SELECT ?x … ORDER BY DESC(?sal)`), or by a SELECT-clause
                // alias. Extend the layout + bindings with the aliases, sort, then
                // project — a stable sort preserves the resulting order.
                if !q.order_by.is_empty() {
                    if let Projection::Items(items) = &q.projection {
                        for it in items {
                            if let SelectItem::Expr(e, alias) = it {
                                if !layout.index.contains_key(alias) {
                                    let idx = layout.names.len();
                                    layout.index.insert(alias.clone(), idx);
                                    layout.names.push(alias.clone());
                                    layout.is_pred.push(false);
                                    for b in solutions.iter_mut() {
                                        // The column exists (unbound) while its
                                        // expression is evaluated, so a reference
                                        // to the alias itself reads unbound
                                        // instead of indexing past the end — as
                                        // the Top-K path (`order_extend`) does.
                                        b.push(None);
                                        b[idx] = self.eval_id(e, b, &layout);
                                    }
                                }
                            }
                        }
                    }
                    self.sort_solutions(&mut solutions, &q.order_by, &layout);
                }
                solutions
                    .iter()
                    .map(|b| self.project_items(b, &layout, q, &result_vars))
                    .collect::<Result<Vec<_>>>()?
            };

        // DISTINCT / OFFSET / LIMIT over the projected result rows (ORDER BY was
        // applied above — before projection for the non-aggregate path).
        Ok((result_vars, is_pred, apply_row_modifiers(q, rows)))
    }

    /// Where each variable of an aggregate query's trailing `VALUES` block
    /// meets the group rows (see [`eval_aggregation`](Self::eval_aggregation)),
    /// adding a column to `glayout` for a variable it does not have.
    fn values_columns(
        &self,
        vars: &[String],
        q: &SelectQuery,
        items: &[SelectItem],
        alias_cols: &[Option<usize>],
        glayout: &mut VarLayout,
    ) -> Vec<ValuesCol> {
        vars.iter()
            .map(|v| {
                let alias = items.iter().zip(alias_cols).find_map(|(it, col)| match it {
                    SelectItem::Expr(_, a) if a == v => *col,
                    _ => None,
                });
                if let Some(col) = alias {
                    return ValuesCol::Alias(col);
                }
                if let Some(&i) = glayout.index.get(v) {
                    let grouped = q
                        .group_by
                        .iter()
                        .any(|e| matches!(e, Expr::Var(g) if g == v))
                        || items
                            .iter()
                            .any(|it| matches!(it, SelectItem::Var(x) if x == v));
                    return if grouped {
                        ValuesCol::Match(i)
                    } else {
                        ValuesCol::Bind(i)
                    };
                }
                let i = glayout.names.len();
                glayout.index.insert(v.clone(), i);
                glayout.names.push(v.clone());
                glayout.is_pred.push(false);
                ValuesCol::Bind(i)
            })
            .collect()
    }

    /// Join one group row with one row of a trailing `VALUES` block over the
    /// [`ValuesCol::Match`] / [`ValuesCol::Bind`] columns: `None` when a
    /// matched column holds a different term, else the row with the VALUES
    /// terms bound (an `UNDEF` cell leaves the column as it is). Alias
    /// columns are checked later, by [`values_alias_join`](Self::values_alias_join).
    fn values_join_row(
        &self,
        gb: &Binding,
        vrow: &[Option<Term>],
        cols: &[ValuesCol],
        glayout: &VarLayout,
    ) -> Option<Binding> {
        let mut merged = gb.clone();
        for (cell, col) in vrow.iter().zip(cols) {
            let Some(term) = cell else { continue };
            match *col {
                ValuesCol::Match(c) => match gb[c] {
                    Some(id) if self.id_to_term(id, glayout.is_pred[c]).as_ref() != Some(term) => {
                        return None
                    }
                    Some(_) => {}
                    None => merged[c] = Some(self.values_cell_id(term, glayout.is_pred[c])),
                },
                ValuesCol::Bind(c) => {
                    merged[c] = Some(self.values_cell_id(term, glayout.is_pred[c]))
                }
                ValuesCol::Alias(_) => {}
            }
        }
        Some(merged)
    }

    /// The [`ValuesCol::Alias`] half of the VALUES join, once the SELECT
    /// expressions are in `gb`: `false` when an alias holds a different term
    /// than the VALUES row; an unbound alias takes the VALUES term.
    fn values_alias_join(
        &self,
        gb: &mut Binding,
        vrow: &[Option<Term>],
        cols: &[ValuesCol],
    ) -> bool {
        for (cell, col) in vrow.iter().zip(cols) {
            let (Some(term), ValuesCol::Alias(c)) = (cell, *col) else {
                continue;
            };
            match gb[c] {
                Some(id) if self.id_to_term(id, false).as_ref() != Some(term) => return false,
                Some(_) => {}
                None => gb[c] = Some(self.term_to_id(term)),
            }
        }
        true
    }

    /// The id a VALUES term takes in a column: a predicate-typed column
    /// resolves its ids through the predicate dictionary, so a term goes
    /// there as a synthetic id (which resolves anywhere).
    fn values_cell_id(&self, term: &Term, is_pred: bool) -> u32 {
        if is_pred {
            self.extras.borrow_mut().intern(term)
        } else {
            self.term_to_id(term)
        }
    }

    /// Project a single solution into result-variable order (no aggregation).
    fn project_items(
        &self,
        b: &Binding,
        layout: &VarLayout,
        q: &SelectQuery,
        result_vars: &[String],
    ) -> Result<Vec<Option<u32>>> {
        match &q.projection {
            Projection::All => Ok(result_vars
                .iter()
                .map(|v| layout.index.get(v).and_then(|&i| b[i]))
                .collect()),
            Projection::Items(items) => items
                .iter()
                .map(|it| match it {
                    SelectItem::Var(v) => Ok(layout.index.get(v).and_then(|&i| b[i])),
                    // Computed up front into the alias column (see
                    // `eval_select_solutions`).
                    SelectItem::Expr(Expr::GraphFn { .. } | Expr::Pfn { .. }, alias) => {
                        Ok(layout.index.get(alias).and_then(|&i| b[i]))
                    }
                    SelectItem::Expr(e, _) => Ok(self.eval_id(e, b, layout)),
                })
                .collect(),
        }
    }

    /// Evaluate a projected advanced graph function over `sols` (all the
    /// WHERE solutions, or one GROUP BY group): resolve each vertex argument
    /// to its distinct bindings across `sols` (a constant IRI to its one
    /// node), the `predSet` against the dictionary, and run the gfun library
    /// on the shared CSR. `constant` is upstream's no-solution path, where a
    /// variable argument is looked up as if it were an IRI and so is a
    /// missing vertex. A variable in `bind_only` ([`bind_only_vars`]) has no
    /// usable binding: upstream keeps BIND results in string columns and
    /// rejects them as vertices ("src must be an entity", GE:2583-2606),
    /// whatever their value. Returns the value's id (a plain-literal JSON
    /// text, an `xsd:boolean` for `cycleBoolean`, …).
    fn eval_graph_fn(
        &self,
        call: &GCall,
        sols: &[&Binding],
        layout: &VarLayout,
        bind_only: &HashSet<String>,
        constant: bool,
    ) -> Result<Option<u32>> {
        let csr = self.gfun_csr()?;
        let nodes = |arg: &Option<VarOrIri>| -> Vec<GNode> {
            match arg {
                None => Vec::new(),
                Some(VarOrIri::Iri(iri)) => vec![gfun::iri_node(&csr, self.dict, iri)],
                Some(VarOrIri::Var(_)) if constant => vec![GNode::MISSING],
                Some(VarOrIri::Var(v)) if bind_only.contains(v) => Vec::new(),
                Some(VarOrIri::Var(v)) => self.distinct_gfn_nodes(v, sols, layout, &csr),
            }
        };
        let (srcs, dsts) = (nodes(&call.src), nodes(&call.dst));
        // A vertex argument whose variable has no usable binding: upstream's
        // `for uid … for vid …` loop never runs, leaving `{"paths":[]}`
        // (`false` for cycleBoolean). Decided here because the library reads
        // an empty `srcs` as "no source given" (clusterCoeff's global form).
        if (call.src.is_some() && srcs.is_empty()) || (call.dst.is_some() && dsts.is_empty()) {
            let value = match call.kind {
                GFnKind::CycleBoolean => Value::Bool(false),
                _ => Value::Str {
                    value: r#"{"paths":[]}"#.into(),
                    lang: None,
                },
            };
            return Ok(Some(self.value_to_id(&value)));
        }
        let mut args = call.to_gargs(srcs, dsts, self.dict);
        args.where_empty = constant;
        let mut ctx = GFunCtx::new(&csr, self.dict).with_max_output_bytes(self.gfun_max_output);
        if let Some(flag) = &self.cancel {
            ctx = ctx.with_cancel(flag);
        }
        let value = match gfun::eval(call.kind, &ctx, &args)? {
            // Upstream stores the JSON text as a plain literal (GE:1578,
            // GE:2019). `confidenceDegree`'s `pfn_type` result replaces the
            // whole upstream response with `{"results": …}`; this engine has
            // no such response mode and returns its JSON as a plain literal.
            GFunValue::Json(text) | GFunValue::ResultsJson(text) => Value::Str {
                value: text,
                lang: None,
            },
            GFunValue::Bool(v) => Value::Bool(v),
            GFunValue::Int(v) => Value::Int(v),
            GFunValue::Num(v) => Value::Double(v),
        };
        Ok(Some(self.value_to_id(&value)))
    }

    /// Evaluate a projected graph function or PFN call (see
    /// [`eval_graph_fn`](Self::eval_graph_fn) / [`eval_pfn`](Self::eval_pfn)).
    fn eval_proj_call(
        &self,
        call: &ProjCall,
        sols: &[&Binding],
        layout: &VarLayout,
        bind_only: &HashSet<String>,
        constant: bool,
    ) -> Result<Option<u32>> {
        match call {
            ProjCall::Graph(c) => self.eval_graph_fn(c, sols, layout, bind_only, constant),
            ProjCall::Pfn { name, json } => {
                self.eval_pfn(name, json, sols, layout, bind_only, constant)
            }
        }
    }

    /// Evaluate a projected `PFN.name({…})` over `sols` as upstream's
    /// `pfnQuery` / `pfnQueryByVar` do ([`crate::pfn::call`]): with
    /// `constant` (no solutions) the function runs once on the argument as
    /// written and its output is the value; otherwise every `"?v"` argument
    /// value takes each distinct term `?v` is bound to across `sols` (a
    /// variable bound only by BIND, or to a computed value, has no usable
    /// binding, as for graph functions) and the value is the JSON array of
    /// the outputs. The value is a plain literal holding that text; the
    /// `/api` layer answers it as upstream's `{"results": …}`. The calls run
    /// with [`with_pfn`](Self::with_pfn)'s caller over this query's CSR.
    fn eval_pfn(
        &self,
        name: &str,
        json: &str,
        sols: &[&Binding],
        layout: &VarLayout,
        bind_only: &HashSet<String>,
        constant: bool,
    ) -> Result<Option<u32>> {
        let args = crate::pfn::call::parse_args(name, json)?;
        let Some(caller) = &self.pfn else {
            let mut msg = format!(
                "PFN.{name}(…): no PFN function registry is attached to this query (functions \
                 are managed per user with the server's funcudb / funquery operations; an \
                 embedded database attaches one with Database::set_pfn_caller)"
            );
            if !crate::pfn::RUNTIME_AVAILABLE {
                msg.push_str(&format!("; besides, {}", crate::pfn::FEATURE_HINT));
            }
            return Err(GStoreError::Query(msg));
        };
        // Looked up once (upstream `getPfnSoFile`, before any call — so a
        // missing or uncompiled function fails even when nothing would run).
        let func = caller.resolve(name).map_err(|e| {
            if constant {
                e
            } else {
                crate::pfn::call::by_var_error(e)
            }
        })?;
        let csr = self.gfun_csr()?;
        let graph = crate::pfn::CsrGraphCtx::new(&csr, self.dict);
        let var_values = |var: &str| -> Option<Vec<String>> {
            let &idx = layout.index.get(var)?;
            if bind_only.contains(var) {
                return Some(Vec::new());
            }
            let mut seen = HashSet::new();
            let mut out = Vec::new();
            for b in sols {
                let Some(id) = b[idx] else { continue };
                if is_synth(id) || !seen.insert(id) {
                    continue;
                }
                if let Some(s) = self.resolve_string(Some(id), layout.is_pred[idx]) {
                    out.push(s);
                }
            }
            Some(out)
        };
        let run = |params: &str| -> Result<String> {
            self.check_cancel()?;
            // Upstream logs each call (`SLOG_CORE`) and a failed one
            // (`SLOG_ERROR("run dynamic function fail: …")`).
            crate::gs_debug!("PFN.{name}: calling with {params}");
            caller
                .run(
                    &func,
                    params,
                    &graph,
                    self.cancel.as_deref(),
                    self.gfun_max_output,
                )
                .inspect_err(|e| {
                    if !matches!(e, GStoreError::Cancelled) {
                        crate::gs_warn!("PFN.{name}: the call failed: {e}");
                    }
                })
        };
        let text = crate::pfn::call::evaluate(
            name,
            &args,
            constant,
            var_values,
            run,
            self.gfun_max_output,
        )?;
        Ok(Some(self.value_to_id(&Value::Str {
            value: text,
            lang: None,
        })))
    }

    /// The distinct vertices a graph-function variable is bound to across
    /// `sols`, in first-appearance order. Unbound rows are skipped (GE:2582
    /// `!= INVALID`); a computed value that is not a stored term (e.g. a
    /// VALUES result outside the dictionary, like upstream's string columns,
    /// "src must be an entity", GE:2577) is skipped (variables bound only by
    /// BIND never get here, see `eval_graph_fn`); a stored literal is
    /// passed with its dictionary id and no CSR vertex, as upstream passes
    /// its literal id (it has no edges and prints as its literal text); a
    /// predicate-position binding is the entity with the same IRI, if any.
    /// A variable the WHERE clause never binds has no bindings.
    fn distinct_gfn_nodes(
        &self,
        var: &str,
        sols: &[&Binding],
        layout: &VarLayout,
        csr: &CsrGraph,
    ) -> Vec<GNode> {
        let Some(&idx) = layout.index.get(var) else {
            return Vec::new();
        };
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for b in sols {
            let Some(id) = b[idx] else { continue };
            if is_synth(id) {
                continue;
            }
            let entity = if layout.is_pred[idx] {
                match self
                    .dict
                    .predicate_to_string(id)
                    .and_then(|key| self.dict.entity_id(key))
                {
                    Some(e) => e,
                    None => continue,
                }
            } else {
                id
            };
            if seen.insert(entity) {
                out.push(GNode::resolve(csr, Some(entity)));
            }
        }
        out
    }

    /// Group solutions and compute the projected rows of an aggregate query
    /// (one per group that passes HAVING, sorted by ORDER BY), following
    /// SPARQL 1.1 §18.2.4.1: every aggregate in the SELECT expressions,
    /// HAVING and ORDER BY is computed once per group into a hidden column
    /// of the group's row, and the expressions — with the aggregates replaced
    /// by those columns — are then ordinary expressions over the row. So
    /// builtins wrapping aggregates (`ROUND(AVG(?x))`, `STR(COUNT(*))`,
    /// `SUM(?a)/COUNT(?a)`, `COALESCE(MIN(?x), 0)`, `IF(COUNT(*) > 1, …)`)
    /// evaluate like any other expression. SELECT expressions are evaluated
    /// in order (a later one may use an earlier alias) and ORDER BY sees the
    /// aliases. A group row binds each GROUP BY variable to the group's key
    /// and, as before, every other WHERE variable to its value in the group's
    /// first solution. `bind_only` is [`bind_only_vars`] of the pattern, for
    /// the projected graph functions.
    fn eval_aggregation(
        &self,
        solutions: &[Binding],
        layout: &VarLayout,
        bind_only: &HashSet<String>,
        q: &SelectQuery,
    ) -> Result<Vec<Vec<Option<u32>>>> {
        let items = match &q.projection {
            Projection::Items(items) => items,
            Projection::All => {
                return Err(GStoreError::Query(
                    "SELECT * with aggregation is not allowed".into(),
                ))
            }
        };

        // Group key = the GROUP BY expression values (empty ⇒ one group). A
        // plain variable keys by its bound term itself (RDF term identity, in
        // the variable's own id space); an expression by its computed value.
        use std::collections::hash_map::Entry;
        let mut order: Vec<Vec<Option<u32>>> = Vec::new();
        let mut groups: HashMap<Vec<Option<u32>>, Vec<usize>> = HashMap::new();
        for (i, b) in solutions.iter().enumerate() {
            let key: Vec<Option<u32>> = q
                .group_by
                .iter()
                .map(|e| match e {
                    Expr::Var(v) => layout.index.get(v).and_then(|&vi| b[vi]),
                    e => self.eval_value(e, b, layout).map(|v| self.value_to_id(&v)),
                })
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

        // §18.2.4.1: lift every aggregate into a hidden group-row column.
        let mut aggs: Vec<Expr> = Vec::new();
        let lifted_items: Vec<Option<Expr>> = items
            .iter()
            .map(|it| match it {
                SelectItem::Expr(Expr::GraphFn { .. } | Expr::Pfn { .. }, _)
                | SelectItem::Var(_) => None,
                SelectItem::Expr(e, _) => Some(lift_aggregates(e, &mut aggs)),
            })
            .collect();
        let having: Vec<Expr> = q
            .having
            .iter()
            .map(|h| lift_aggregates(h, &mut aggs))
            .collect();
        let order_by: Vec<OrderCondition> = q
            .order_by
            .iter()
            .map(|o| OrderCondition {
                expr: lift_aggregates(&o.expr, &mut aggs),
                descending: o.descending,
            })
            .collect();
        let mut glayout = group_layout(layout, aggs.len());
        // One column per SELECT expression alias.
        let mut alias_cols = Vec::with_capacity(items.len());
        for it in items {
            alias_cols.push(match it {
                SelectItem::Var(_) => None,
                SelectItem::Expr(_, alias) => Some(match glayout.index.get(alias) {
                    Some(&i) => {
                        glayout.is_pred[i] = false;
                        i
                    }
                    None => {
                        let i = glayout.names.len();
                        glayout.index.insert(alias.clone(), i);
                        glayout.names.push(alias.clone());
                        glayout.is_pred.push(false);
                        i
                    }
                }),
            });
        }

        // A trailing VALUES block (§18.2.4.3) joins the group rows after
        // HAVING: on the GROUP BY and projected variables, and on the SELECT
        // aliases once they are computed (as the join of the projected rows
        // would); any other VALUES variable is not in scope after grouping,
        // so the VALUES row binds it, visible to the SELECT expressions.
        let values_cols = match &q.values {
            Some((vars, _)) => self.values_columns(vars, q, items, &alias_cols, &mut glayout),
            None => Vec::new(),
        };

        let mut grows: Vec<Binding> = Vec::new();
        for key in &order {
            let sols: Vec<&Binding> = groups[key].iter().map(|&i| &solutions[i]).collect();
            let gb = self.group_row(&sols, layout, &glayout, &aggs);
            // HAVING: drop groups failing any constraint.
            if !having
                .iter()
                .all(|h| self.eval_ebv(h, &gb, &glayout) == Some(true))
            {
                continue;
            }
            // An advanced graph function over the group's solutions
            // (upstream: once per GROUP BY group, GE:2546-2620).
            let mut gfn_values: Vec<Option<Option<u32>>> = Vec::with_capacity(items.len());
            for it in items {
                gfn_values.push(match it {
                    SelectItem::Expr(Expr::GraphFn { func, args }, _) => {
                        let call = GCall::from_ast(*func, args)?;
                        Some(self.eval_graph_fn(&call, &sols, layout, bind_only, false)?)
                    }
                    SelectItem::Expr(Expr::Pfn { name, json }, _) => {
                        Some(self.eval_pfn(name, json, &sols, layout, bind_only, false)?)
                    }
                    _ => None,
                });
            }
            let joined: Vec<(Binding, Option<&Vec<Option<Term>>>)> = match &q.values {
                None => vec![(gb, None)],
                Some((_, vrows)) => vrows
                    .iter()
                    .filter_map(|vrow| {
                        self.values_join_row(&gb, vrow, &values_cols, &glayout)
                            .map(|row| (row, Some(vrow)))
                    })
                    .collect(),
            };
            for (mut gb, vrow) in joined {
                for ((lifted, gfn), col) in lifted_items.iter().zip(&gfn_values).zip(&alias_cols) {
                    let Some(col) = *col else { continue };
                    gb[col] = match (gfn, lifted) {
                        (Some(v), _) => *v,
                        (None, Some(e)) => self.eval_id(e, &gb, &glayout),
                        (None, None) => None,
                    };
                }
                if let Some(vrow) = vrow {
                    if !self.values_alias_join(&mut gb, vrow, &values_cols) {
                        continue;
                    }
                }
                grows.push(gb);
            }
        }
        if !order_by.is_empty() {
            self.sort_solutions(&mut grows, &order_by, &glayout);
        }
        Ok(grows
            .iter()
            .map(|gb| {
                items
                    .iter()
                    .zip(&alias_cols)
                    .map(|(it, col)| match (it, col) {
                        (SelectItem::Var(v), _) => glayout.index.get(v).and_then(|&i| gb[i]),
                        (_, Some(col)) => gb[*col],
                        (_, None) => None,
                    })
                    .collect()
            })
            .collect())
    }

    /// A group's row (see [`eval_aggregation`](Self::eval_aggregation)) over
    /// `glayout` (the WHERE `layout` extended by [`group_layout`] and maybe
    /// alias columns): the group's first solution, then the value of each
    /// lifted aggregate of `aggs` (an error leaves its column unbound).
    fn group_row(
        &self,
        sols: &[&Binding],
        layout: &VarLayout,
        glayout: &VarLayout,
        aggs: &[Expr],
    ) -> Binding {
        let mut gb: Binding = match sols.first() {
            Some(b) => (*b).clone(),
            None => vec![None; layout.len()],
        };
        gb.resize(glayout.len(), None);
        for (i, a) in aggs.iter().enumerate() {
            if let Expr::Aggregate {
                func,
                distinct,
                arg,
                sep,
            } = a
            {
                gb[layout.len() + i] = self
                    .eval_aggregate(
                        *func,
                        *distinct,
                        arg.as_deref(),
                        sep.as_deref(),
                        sols,
                        layout,
                    )
                    .map(|v| self.value_to_id(&v));
            }
        }
        gb
    }

    /// HAVING over all of `sols` as one group (no GROUP BY) — upstream treats
    /// a projected graph function as an aggregate, so its HAVING applies.
    fn having_holds(&self, q: &SelectQuery, sols: &[&Binding], layout: &VarLayout) -> bool {
        if q.having.is_empty() {
            return true;
        }
        let mut aggs = Vec::new();
        let having: Vec<Expr> = q
            .having
            .iter()
            .map(|h| lift_aggregates(h, &mut aggs))
            .collect();
        let glayout = group_layout(layout, aggs.len());
        let gb = self.group_row(sols, layout, &glayout, &aggs);
        having
            .iter()
            .all(|h| self.eval_ebv(h, &gb, &glayout) == Some(true))
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
        // solution mappings (SPARQL 1.1 §18.5.1.1), so dedup bindings before
        // counting when the DISTINCT modifier is present. A mapping holds
        // only the query's own variables: the hidden variables of `[]` /
        // `( … )` (existential blank nodes, which `SELECT DISTINCT *` leaves
        // out too) and a mixed predicate variable's predicate-space twin are
        // not part of it.
        if func == AggFunc::Count && arg.is_none() {
            if distinct {
                let visible: Vec<usize> = (0..layout.len())
                    .filter(|&i| {
                        let name = &layout.names[i];
                        !is_anon_var(name) && !name.starts_with(PRED_ALIAS_PREFIX)
                    })
                    .collect();
                let mut seen: HashSet<Vec<Option<u32>>> = HashSet::new();
                let n = sols
                    .iter()
                    .filter(|b| {
                        seen.insert(
                            visible
                                .iter()
                                .map(|&i| b.get(i).copied().flatten())
                                .collect(),
                        )
                    })
                    .count();
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
            // Avg(S) = Sum(S) / Count(S) with the query's division (so an
            // integer average is a decimal, or truncated in upstream mode);
            // 0 for an empty group (§18.5.1.4).
            AggFunc::Avg => {
                if vals.is_empty() {
                    Value::Int(0)
                } else {
                    sum_values(&vals).arith_with(
                        ArithOp::Div,
                        &Value::Int(vals.len() as i64),
                        self.integer_division,
                    )?
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
                // When the filtered pattern is a plain BGP, push mandatory-substring
                // constraints (CONTAINS/STRSTARTS/…) into candidate generation so
                // the StringIndex can prune before the join. The exact filters below
                // still run, so this is purely an acceleration.
                let mut r = match inner.as_ref() {
                    GraphPattern::Bgp(tps) => {
                        let cons = self.filter_string_constraints(filters, layout);
                        self.eval_bgp_constrained(tps, layout, &cons)
                    }
                    _ => self.eval_pattern(inner, layout),
                };
                r.retain(|b| {
                    // Cooperative cancellation between rows: once requested, drop
                    // remaining rows instead of evaluating (possibly expensive)
                    // filters; the Result-returning caller turns the truncated
                    // result into GStoreError::Cancelled via check_cancel().
                    if self.cancelled() {
                        return false;
                    }
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
                    if let Some(id) = self.eval_id(expr, b, layout) {
                        b[vi] = Some(id);
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

        // Refused outright when this evaluator may not contact remote
        // endpoints (reported as an error by `check_cancel`).
        if !self.allow_service {
            self.service_denied.set(true);
            return Vec::new();
        }

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
            cancel: self.cancel.clone(), // cancellation propagates into GRAPH blocks
            algo_cache: std::cell::RefCell::new(AlgoCache::default()), // per-graph, not shared
            string_index: std::cell::RefCell::new(None), // per-graph store, own index
            string_index_tried: std::cell::Cell::new(false),
            allow_service: self.allow_service,
            service_denied: Rc::clone(&self.service_denied),
            failed: Rc::clone(&self.failed),
            // Graph functions inside a GRAPH block run over that named graph:
            // its CSR is built from the sub-evaluator's own store.
            csr_provider: None,
            gfun_csr: std::cell::RefCell::new(None),
            integer_division: self.integer_division,
            topk: self.topk,
            stats: Rc::clone(&self.stats),
            gfun_max_output: self.gfun_max_output,
            pfn: self.pfn.clone(),
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
            Err(e) => {
                // Reported as the query's error (see `failed`), never as
                // "no solution" — which would invert a `NOT EXISTS`.
                let mut failed = self.failed.borrow_mut();
                if failed.is_none() {
                    *failed = Some(e);
                }
                return Vec::new();
            }
        };
        // Map each sub-query result column to an outer layout slot (by name).
        let slots: Vec<Option<usize>> = vars.iter().map(|v| layout.index.get(v).copied()).collect();
        rows.into_iter()
            .map(|r| {
                let mut b = vec![None; layout.len()];
                for (col, id) in r.into_iter().enumerate() {
                    if let Some(idx) = slots[col] {
                        // A predicate-typed inner column carries a *predicate*-
                        // dictionary id, but a variable a sub-SELECT binds is
                        // never predicate-typed in the outer layout (it is
                        // entity-space, or mixed, see `VarLayout::build`).
                        // Re-mint such a value in the entity space with
                        // `term_to_id`: the term's dictionary id when it has
                        // one — so it joins with the same IRI bound elsewhere
                        // — else a synthetic id (idempotent per term).
                        b[idx] = match id {
                            Some(pid) if is_pred[col] && !is_synth(pid) => {
                                self.id_to_term(pid, true).map(|t| self.term_to_id(&t))
                            }
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
        let mut parts: Vec<Option<(Vec<Binding>, Vec<usize>)>> = conjuncts
            .iter()
            .map(|c| Some((self.eval_pattern(c, layout), pattern_vars(c, layout))))
            .collect();

        // Start from the smallest result, then greedily join in a connected,
        // smallest-first order (a lightweight join optimizer).
        let meta: Vec<(usize, &[usize])> = parts
            .iter()
            .map(|p| {
                p.as_ref()
                    .map(|(b, v)| (b.len(), v.as_slice()))
                    .unwrap_or_default()
            })
            .collect();
        let order = greedy_join_order(&meta);
        let mut order = order.into_iter();
        let (mut acc, mut acc_vars) = order
            .next()
            .and_then(|i| parts[i].take())
            .unwrap_or_default();
        for pick in order {
            let (rb, rv) = parts[pick].take().unwrap_or_default();
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
        self.eval_bgp_constrained(tps, layout, &HashMap::new())
    }

    /// [`eval_bgp`](Self::eval_bgp) with mandatory-substring `FILTER` constraints
    /// (variable index → substrings its value must contain) pushed down into
    /// candidate generation, so `FILTER(CONTAINS(?v, "…"))` prunes/seeds `?v`'s
    /// candidates via the dictionary [`StringIndex`] before the join. The exact
    /// filter still runs afterwards, so this only reduces work, never results.
    fn eval_bgp_constrained(
        &self,
        tps: &[TriplePattern],
        layout: &VarLayout,
        str_constraints: &HashMap<usize, Vec<String>>,
    ) -> Vec<Binding> {
        let (plans, candidates, plan) = match self.prepare_bgp(tps, layout, str_constraints) {
            PreparedBgp::Empty => return Vec::new(),
            PreparedBgp::Identity => return vec![vec![None; layout.len()]],
            PreparedBgp::Plan(p) => (p.plans, p.cands, p.exec),
        };

        // A mixed variable in predicate position (see `VarLayout::pred_alias`)
        // joins across id spaces: run the left-deep order with translation.
        // (The streaming executor in `topk` does the same, depth first.)
        let mixed = mixed_slots(&plans, layout);
        if !mixed.is_empty() {
            return self.eval_mixed_pipeline(&plans, &plan.order, &candidates, layout, &mixed);
        }

        if let Some(tree) = &plan.tree {
            // Bushy execution: build each side independently, then hash-join.
            let (sols, _vars) = self.eval_join_tree(tree, &plans, &candidates, layout);
            return sols;
        }

        // Left-deep pipeline: extend partial bindings one pattern at a time.
        let mut solutions: Vec<Binding> = vec![vec![None; layout.len()]];
        for &pi in &plan.order {
            // Cooperative cancellation: bail between join patterns (short-circuit
            // to empty; the Result-returning caller turns this into Cancelled).
            if self.cancelled() {
                return Vec::new();
            }
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

    /// Compile a BGP and choose its execution plan — everything
    /// [`eval_bgp_constrained`](Self::eval_bgp_constrained) does before it
    /// runs the joins, shared with the streaming executor (`topk`) so both
    /// enumerate the solutions in the same order.
    fn prepare_bgp(
        &self,
        tps: &[TriplePattern],
        layout: &VarLayout,
        str_constraints: &HashMap<usize, Vec<String>>,
    ) -> PreparedBgp {
        // Compile patterns; a missing constant ⇒ the conjunction is empty.
        let mut plans = Vec::with_capacity(tps.len());
        for tp in tps {
            match self.compile_pattern(tp, layout) {
                Some(plan) => plans.push(plan),
                None => return PreparedBgp::Empty,
            }
        }
        if plans.is_empty() {
            return PreparedBgp::Identity;
        }

        // Exact per-variable candidate sets from constant edges (gStore's
        // FilterPlan/CompleteCandidate), optionally refined by the VS-tree.
        let mut candidates = candidates::generate(&plans, self.store);
        self.refine_candidates_with_vstree(&mut candidates, tps, layout);
        // StringIndex: mandatory-substring filters (CONTAINS/STRSTARTS/STRENDS/
        // plain REGEX) prune or seed candidate sets — a sound superset, so only
        // the join shrinks (the exact FILTER still verifies each row).
        if !str_constraints.is_empty() {
            if let Some(idx) = self.string_index() {
                candidates::refine_with_substrings(&mut candidates, str_constraints, &idx, &plans);
            }
        }

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
        PreparedBgp::Plan(topk::BgpPlan {
            plans,
            cands: candidates,
            exec: plan,
        })
    }

    /// The left-deep pipeline of a BGP in which a *mixed* variable `x` (bound
    /// in predicate position and some other way, e.g. `?x ?p ?o . ?s ?x ?y`)
    /// occurs in predicate position; `mixed` lists its `(x, hidden)` slot
    /// pairs ([`VarLayout::pred_alias`]). Predicate positions bind the hidden
    /// slot with predicate-dictionary ids, every other position binds `x`
    /// with entity/literal ids, and the two must name the same RDF term:
    ///
    /// * before a pattern binds the hidden slot, a bound `x` is translated
    ///   into it (no predicate with that IRI ⇒ the row cannot match);
    /// * after each pattern, a bound hidden slot is translated back into `x`
    ///   (bound if unbound, else it must agree).
    ///
    /// The hidden slots are cleared at the end, so the solutions carry `x`
    /// only, in the entity space its layout entry says. Joins thus stay
    /// index-driven instead of a cross product filtered afterwards.
    fn eval_mixed_pipeline(
        &self,
        plans: &[PatPlan],
        order: &[usize],
        cands: &Candidates,
        layout: &VarLayout,
        mixed: &[(usize, usize)],
    ) -> Vec<Binding> {
        let mut caches = MixedCaches::default();
        let mut solutions: Vec<Binding> = vec![vec![None; layout.len()]];
        let mut fresh = Vec::new();
        for &pi in order {
            if self.cancelled() {
                return Vec::new();
            }
            let plan = &plans[pi];
            let mut next = Vec::new();
            for binding in &solutions {
                let mut b = binding.clone();
                if !self.mixed_enter(&mut b, plan, mixed, &mut caches) {
                    continue;
                }
                self.extend(&b, plan, cands, &mut fresh);
                for mut nb in fresh.drain(..) {
                    if self.mixed_leave(&mut nb, mixed, &mut caches) {
                        next.push(nb);
                    }
                }
            }
            solutions = next;
            if solutions.is_empty() {
                break;
            }
        }
        for b in &mut solutions {
            mixed_clear(b, mixed);
        }
        solutions
    }

    /// Before `plan` extends `b` in a mixed pipeline
    /// ([`eval_mixed_pipeline`](Self::eval_mixed_pipeline)): translate each
    /// bound mixed variable whose hidden slot `plan` binds into that slot.
    /// `false` when its IRI names no predicate (the row cannot match).
    fn mixed_enter(
        &self,
        b: &mut Binding,
        plan: &PatPlan,
        mixed: &[(usize, usize)],
        caches: &mut MixedCaches,
    ) -> bool {
        for &(x, h) in mixed {
            let (Some(id), None, Slot::Var(p)) = (b[x], b[h], plan.p) else {
                continue;
            };
            if p != h {
                continue;
            }
            let pid = *caches
                .to_pred
                .entry(id)
                .or_insert_with(|| self.iri_predicate_id(id));
            match pid {
                Some(pid) => b[h] = Some(pid),
                None => return false,
            }
        }
        true
    }

    /// After a pattern extended a row of a mixed pipeline: translate each
    /// bound hidden slot back into its mixed variable (bound if unbound,
    /// else it must agree). `false` when they disagree (drop the row).
    fn mixed_leave(
        &self,
        nb: &mut Binding,
        mixed: &[(usize, usize)],
        caches: &mut MixedCaches,
    ) -> bool {
        for &(x, h) in mixed {
            let Some(pid) = nb[h] else { continue };
            let node = *caches
                .to_node
                .entry(pid)
                .or_insert_with(|| self.id_to_term(pid, true).map(|t| self.term_to_id(&t)));
            match (node, nb[x]) {
                (Some(n), None) => nb[x] = Some(n),
                (Some(n), Some(cur)) if n == cur => {}
                _ => return false,
            }
        }
        true
    }

    /// The predicate-dictionary id of the IRI an entity-space `id` names, if
    /// that IRI is a predicate (for [`eval_mixed_pipeline`](Self::eval_mixed_pipeline)).
    fn iri_predicate_id(&self, id: u32) -> Option<u32> {
        match self.id_to_term(id, false)? {
            t @ Term::Iri(_) => self.dict.predicate_id(&t.dict_key()),
            _ => None,
        }
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
                self.note_bindings(joined.len());
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

    /// The dictionary substring index, built once per query on first use (see the
    /// [`string_index`](Self#structfield.string_index) field). `None` when it
    /// would not help: a disk-backed dictionary (no in-memory strings ⇒ empty
    /// entries) or one too small for indexing to beat a linear scan.
    ///
    /// The index is keyed by each term's **`STR()`/lexical value** — the exact
    /// string a substring `FILTER` compares against — NOT the raw N-Triples dict
    /// key. This keeps candidate pruning sound: indexing the escaped/quoted dict
    /// form would miss a needle containing an escaped character (`"`, `\`, a
    /// newline) or a numeric whose canonical lexical differs from its stored form,
    /// silently dropping true matches. A term whose dict string somehow fails to
    /// re-parse falls back to its raw form (a term the dictionary stored always
    /// re-parses, so this is only a safety net).
    fn string_index(&self) -> Option<Rc<StringIndex>> {
        if !self.string_index_tried.get() {
            self.string_index_tried.set(true);
            let raw = self.dict.entity_literal_entries();
            if raw.len() >= STRING_INDEX_MIN_ENTRIES {
                let lexical: Vec<(u32, String)> = raw
                    .iter()
                    .map(|(id, s)| {
                        let key = key_term(s)
                            .map(|t| Value::from_term(&t).lexical())
                            .unwrap_or_else(|| (*s).to_string());
                        (*id, key)
                    })
                    .collect();
                *self.string_index.borrow_mut() = Some(Rc::new(StringIndex::build(lexical)));
            }
        }
        self.string_index.borrow().clone()
    }

    /// Extract mandatory-substring constraints (variable index → substrings) from
    /// `FILTER` expressions, for [`StringIndex`]-accelerated candidate pruning.
    /// Only conjuncts whose truth *requires* the variable's string value to hold a
    /// fixed ≥3-byte substring are collected — `CONTAINS`/`STRSTARTS`/`STRENDS` of
    /// `?v` or `STR(?v)` against a literal, and metacharacter-free `REGEX(?v,
    /// "lit")` — so the index candidates stay a sound superset.
    fn filter_string_constraints(
        &self,
        filters: &[Expr],
        layout: &VarLayout,
    ) -> HashMap<usize, Vec<String>> {
        let mut out: HashMap<usize, Vec<String>> = HashMap::new();
        for f in filters {
            collect_substr_constraints(f, layout, &mut out);
        }
        out
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
            // Probe refinement: when the variable already has a small exact
            // candidate set and the tree is in memory, test each candidate's
            // stored signature (O(1) each through the tree's locator) instead
            // of walking the tree. The result is exactly walk ∩ existing: a
            // walk reports precisely the entries whose signature contains the
            // query (every ancestor contains its descendants' signatures).
            let probe_limit = PROBE_MIN_CANDIDATES.max(vstree.entity_count() / 32);
            if let Some(existing) = candidates.get_mut(&vidx) {
                if vstree.can_probe() && existing.len() <= probe_limit {
                    if !vstree.is_empty() {
                        existing.retain(|&x| vstree.probe(x, &sig));
                    }
                    continue;
                }
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
            // A mixed variable binds its hidden predicate-space slot here.
            PatternTerm::Var(v) => {
                let i = layout.index[v];
                Some(Slot::Var(layout.pred_alias.get(&i).copied().unwrap_or(i)))
            }
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
        let before = out.len();
        // The subject's / object's candidate sets, looked up once per call.
        let s_cands = slot_candidates(&plan.s, cands);
        let o_cands = slot_candidates(&plan.o, cands);
        for (s, p, o) in self.candidate_triples_in(binding, plan, s_cands, o_cands) {
            // VS-tree filter: an entity variable's value must be a candidate.
            if !in_candidates(s_cands, s) || !in_candidates(o_cands, o) {
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
        self.note_bindings(out.len() - before);
    }

    /// [`candidate_triples`](Self::candidate_triples) for a caller that keeps
    /// only triples whose subject is in `s_cands` and object in `o_cands`
    /// (`None` = unconstrained): a predicate scan (`? p ?`) whose subject or
    /// object variable has a candidate set much smaller than the predicate is
    /// answered by looking each candidate up instead of scanning the
    /// predicate. Exactly the scanned triples that pass the candidate filter
    /// are returned (possibly with others the caller then drops), in the same
    /// `(s, o)` order, so solutions and their order are unchanged.
    fn candidate_triples_in(
        &self,
        binding: &Binding,
        plan: &PatPlan,
        s_cands: Option<&[u32]>,
        o_cands: Option<&[u32]>,
    ) -> Vec<(u32, PredId, u32)> {
        let known = (
            slot_known(&plan.s, binding),
            slot_known(&plan.p, binding),
            slot_known(&plan.o, binding),
        );
        if let (None, Some(p), None) = known {
            let card = self.store.pred_card(p);
            let small = |c: &[u32]| c.len().saturating_mul(CANDIDATE_LOOKUP_FACTOR) < card;
            if let Some(subjects) = s_cands.filter(|c| small(c)) {
                // Ascending subjects, each with its ascending objects: the
                // `(s, o)` order of `so_by_p`.
                let mut out = Vec::new();
                for &s in subjects {
                    out.extend(self.store.o_by_sp(s, p).into_iter().map(|o| (s, p, o)));
                }
                return out;
            }
            if let Some(objects) = o_cands.filter(|c| small(c)) {
                let mut out = Vec::new();
                for &o in objects {
                    out.extend(self.store.s_by_po(p, o).into_iter().map(|s| (s, p, o)));
                }
                // Back into `so_by_p`'s `(s, o)` order.
                out.sort_unstable_by_key(|&(s, _, o)| (s, o));
                return out;
            }
        }
        self.candidate_triples(binding, plan)
    }

    /// The `(s, p, o)` triples that may extend `binding` by `plan`, in the
    /// order of the tightest index for the positions `binding` already fixes.
    /// This order is what makes the executor's solution order deterministic.
    fn candidate_triples(&self, binding: &Binding, plan: &PatPlan) -> Vec<(u32, PredId, u32)> {
        let ks = slot_known(&plan.s, binding);
        let kp = slot_known(&plan.p, binding);
        let ko = slot_known(&plan.o, binding);

        // Enumerate candidate (s, p, o) triples from the tightest index.
        match (ks, kp, ko) {
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
        let term = key_term(s)?;
        Some(Value::from_term(&term))
    }

    /// Evaluate an expression to the RDF term it denotes, for what sees
    /// terms rather than values — `STR`, `DATATYPE`, `sameTerm` and the copy
    /// `BIND(… AS ?v)` / `(… AS ?v)` binds. A variable denotes the term it is
    /// bound to and a constant itself (so `"05"^^xsd:unsignedByte` keeps its
    /// lexical form and datatype, which its [`Value`], an integer, does
    /// not), as does the `COALESCE` / `IF` argument chosen; anything else
    /// the term of its computed value.
    fn eval_term(&self, e: &Expr, b: &Binding, l: &VarLayout) -> Option<Term> {
        match e {
            Expr::Var(name) => {
                let idx = *l.index.get(name)?;
                self.id_to_term((*b.get(idx)?)?, l.is_pred[idx])
            }
            Expr::Const(t) => Some(t.clone()),
            Expr::Builtin(name, args) if name == "COALESCE" => {
                args.iter().find_map(|a| self.eval_term(a, b, l))
            }
            Expr::Builtin(name, args) if name == "IF" => match args.as_slice() {
                [c, then, other] => {
                    if self.eval_ebv(c, b, l)? {
                        self.eval_term(then, b, l)
                    } else {
                        self.eval_term(other, b, l)
                    }
                }
                _ => None,
            },
            _ => self.eval_value(e, b, l).map(|v| value_to_term(&v)),
        }
    }

    /// The id of the term [`eval_term`](Self::eval_term) gives, for a column
    /// that holds entity/literal-space ids: a variable's own id when it is
    /// in that space (so a copy is the very same term), else the term's.
    fn eval_id(&self, e: &Expr, b: &Binding, l: &VarLayout) -> Option<u32> {
        if let Expr::Var(name) = e {
            let idx = *l.index.get(name)?;
            if !l.is_pred[idx] {
                return *b.get(idx)?;
            }
        }
        self.eval_term(e, b, l).map(|t| self.term_to_id(&t))
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
            Expr::In {
                expr,
                list,
                negated,
            } => eval_in(expr, list, *negated, |e| self.eval_value(e, b, l)),
            Expr::Arith(op, x, y) => {
                let a = self.eval_value(x, b, l)?;
                let c = self.eval_value(y, b, l)?;
                a.arith_with(*op, &c, self.integer_division)
            }
            Expr::Unary(op, x) => {
                let v = self.eval_value(x, b, l)?;
                match op {
                    // op:numeric-unary-plus: a type error for a non-number.
                    UnaryOp::Plus => v.is_numeric().then_some(v),
                    UnaryOp::Neg => v.negate(),
                }
            }
            Expr::Builtin(name, args) => self.eval_builtin(name, args, b, l),
            // `rs:NAME(…)` is the legacy gstore-rust built-in NAME (unchanged
            // semantics); `xsd:T(x)` is a SPARQL 1.1 cast; any other IRI is a
            // user-defined function (checked before evaluation, see
            // `check_iri_call`). The bare `SIMILARITY` is upstream's string
            // similarity, so the legacy node/string SIMILARITY is only
            // reachable as `rs:SIMILARITY`.
            Expr::IriCall { iri, args } => match rs_function_name(iri) {
                Some("SIMILARITY") => self.eval_legacy_similarity(args, b, l),
                Some(name) => self.eval_builtin(name, args, b, l),
                None => match XsdCast::from_iri(iri) {
                    Some(target) => match args.as_slice() {
                        [arg] => xsd_cast(target, &self.eval_value(arg, b, l)?),
                        _ => None,
                    },
                    None => self.eval_iri_function(iri, args, b, l),
                },
            },
            // An advanced graph function only evaluates as a whole SELECT
            // projection (`eval_graph_fn`); elsewhere, and PFN anywhere, the
            // query is rejected before evaluation starts (`check_calls`).
            Expr::GraphFn { .. } | Expr::Pfn { .. } => None,
            Expr::Exists(neg, pat) => Some(Value::Bool(self.eval_exists(*neg, pat, b, l))),
            // Aggregates only have meaning in group context (handled elsewhere).
            Expr::Aggregate { .. } => None,
        }
    }

    /// `rs:SIMILARITY(a, b)`, the legacy gstore-rust SIMILARITY. Polymorphic,
    /// but dispatched on IRI-ness (NOT mere dictionary membership): node
    /// similarity (neighbour-set cosine overlap) only when BOTH arguments are
    /// IRIs naming graph nodes; otherwise Jaro string similarity of the
    /// lexical forms. This keeps string similarity working for literals — a
    /// stored literal must never be silently reinterpreted as a graph node
    /// (which would make SIMILARITY on data-bound strings data-dependent and
    /// non-compositional).
    fn eval_legacy_similarity(&self, args: &[Expr], b: &Binding, l: &VarLayout) -> Option<Value> {
        let av = self.eval_value(args.first()?, b, l)?;
        let cv = self.eval_value(args.get(1)?, b, l)?;
        if let (Value::Iri(_), Value::Iri(_)) = (&av, &cv) {
            if let (Some(a), Some(c)) = (
                self.dict.term_id(&value_to_term(&av)),
                self.dict.term_id(&value_to_term(&cv)),
            ) {
                if let Some(sim) = self.graph_view().neighbor_cosine_similarity(a, c) {
                    return Some(Value::Double(sim));
                }
            }
        }
        Some(Value::Double(jaro_similarity(&av.lexical(), &cv.lexical())))
    }

    /// `EXISTS { pat }` / `NOT EXISTS { pat }`: true iff `pat` has at least one
    /// solution compatible with the current binding `b`. We substitute `b`'s
    /// bound variables into `pat` as constants (so inner FILTER/BIND that
    /// reference outer variables see their values), then test non-emptiness.
    fn eval_exists(
        &self,
        negated: bool,
        pat: &GraphPattern,
        b: &Binding,
        outer: &VarLayout,
    ) -> bool {
        let bound = self.subst_pattern(pat, b, outer);
        let layout = VarLayout::build(&bound);
        let found = self.pattern_has_solution(&bound, &layout);
        found ^ negated
    }

    /// An id resolved back to its RDF term (synthetic, predicate, or
    /// entity/literal — routed by `is_pred` exactly as [`resolve_string`]).
    fn id_to_term(&self, id: u32, is_pred: bool) -> Option<Term> {
        if is_synth(id) {
            return self.extras.borrow().term(id).cloned();
        }
        let s = if is_pred {
            self.dict.predicate_to_string(id)?
        } else {
            self.dict.id_to_string(id)?
        };
        key_term(s)
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
            Expr::In {
                expr,
                list,
                negated,
            } => Expr::In {
                expr: Box::new(self.subst_expr(expr, b, layout)),
                list: list.iter().map(|a| self.subst_expr(a, b, layout)).collect(),
                negated: *negated,
            },
            Expr::Arith(op, x, y) => Expr::Arith(
                *op,
                Box::new(self.subst_expr(x, b, layout)),
                Box::new(self.subst_expr(y, b, layout)),
            ),
            Expr::Builtin(n, args) => Expr::Builtin(
                n.clone(),
                args.iter().map(|a| self.subst_expr(a, b, layout)).collect(),
            ),
            Expr::IriCall { iri, args } => Expr::IriCall {
                iri: iri.clone(),
                args: args.iter().map(|a| self.subst_expr(a, b, layout)).collect(),
            },
            Expr::GraphFn { .. } | Expr::Pfn { .. } => e.clone(),
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
                arg: arg
                    .as_ref()
                    .map(|a| Box::new(self.subst_expr(a, b, layout))),
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
        // Names outside the built-in table are user-defined functions.
        let Some(spec) = builtins::lookup(name) else {
            return self.eval_custom_function(name, args, b, l);
        };
        // The parser rejects wrong-arity built-in calls; an AST built some other
        // way gets an evaluation error (solution dropped), never an index panic.
        if !spec.arity.accepts(args.len()) {
            return None;
        }
        // BOUND inspects binding state directly (its arg may be unbound).
        if name == "BOUND" {
            if let [Expr::Var(v)] = args {
                let idx = *l.index.get(v)?;
                return Some(Value::Bool(b[idx].is_some()));
            }
            return None;
        }
        match name {
            // The term's own lexical form ("05"^^xsd:unsignedByte is "05").
            "STR" => Some(Value::Str {
                value: match self.eval_term(args.first()?, b, l)? {
                    Term::Literal { value, .. } => value,
                    Term::Iri(s) | Term::Blank(s) => s,
                },
                lang: None,
            }),
            "STRLEN" => {
                let v = self.eval_value(args.first()?, b, l)?;
                Some(Value::Int(v.lexical().chars().count() as i64))
            }
            "UCASE" => self.str1(args, b, l, |s| s.to_uppercase()),
            "LCASE" => self.str1(args, b, l, |s| s.to_lowercase()),
            // Type-preserving (an integer stays an integer, a decimal a
            // decimal, a float a float); `ROUND` rounds halves upward.
            "ABS" => self
                .eval_value(args.first()?, b, l)?
                .numeric_fn(NumericFn::Abs),
            "CEIL" => self
                .eval_value(args.first()?, b, l)?
                .numeric_fn(NumericFn::Ceil),
            "FLOOR" => self
                .eval_value(args.first()?, b, l)?
                .numeric_fn(NumericFn::Floor),
            "ROUND" => self
                .eval_value(args.first()?, b, l)?
                .numeric_fn(NumericFn::Round),
            "ISNUMERIC" => Some(Value::Bool(
                self.eval_value(args.first()?, b, l)?.is_numeric(),
            )),
            "ISIRI" | "ISURI" => Some(Value::Bool(matches!(
                self.eval_value(args.first()?, b, l)?,
                Value::Iri(_)
            ))),
            "ISBLANK" => Some(Value::Bool(matches!(
                self.eval_value(args.first()?, b, l)?,
                Value::Blank(_)
            ))),
            "ISLITERAL" => Some(Value::Bool(matches!(
                self.eval_value(args.first()?, b, l)?,
                Value::Int(_)
                    | Value::BigInt(_)
                    | Value::Decimal(_)
                    | Value::Float(_)
                    | Value::Double(_)
                    | Value::Bool(_)
                    | Value::Str { .. }
                    | Value::Typed { .. }
            ))),
            "LANG" => {
                let v = self.eval_value(args.first()?, b, l)?;
                let lang = match v {
                    Value::Str { lang, .. } => lang.unwrap_or_default(),
                    _ => String::new(),
                };
                Some(Value::Str {
                    value: lang,
                    lang: None,
                })
            }
            // The term's own datatype ("5"^^xsd:unsignedByte is
            // xsd:unsignedByte); IRIs and blank nodes have none.
            "DATATYPE" => match self.eval_term(args.first()?, b, l)? {
                Term::Literal { lang: Some(_), .. } => Some(Value::Iri(RDF_LANG_STRING.into())),
                Term::Literal { datatype, .. } => Some(Value::Iri(
                    datatype.unwrap_or_else(|| xsd::STRING.to_owned()),
                )),
                Term::Iri(_) | Term::Blank(_) => None,
            },
            "CONTAINS" => self.str2_bool(args, b, l, |a, c| a.contains(&c)),
            "STRSTARTS" => self.str2_bool(args, b, l, |a, c| a.starts_with(&c)),
            "STRENDS" => self.str2_bool(args, b, l, |a, c| a.ends_with(&c)),
            "STRBEFORE" => self.str_split(args, b, l, true),
            "STRAFTER" => self.str_split(args, b, l, false),
            // §17.4.3.12: the language tag survives when every argument
            // carries the same one; otherwise the result is a simple literal.
            "CONCAT" => {
                let mut out = String::new();
                let mut common: Option<Option<String>> = None;
                for a in args {
                    let (s, lang) = str_and_lang(&self.eval_value(a, b, l)?);
                    out.push_str(&s);
                    common = match common {
                        None => Some(lang),
                        Some(c) if c == lang => Some(c),
                        Some(_) => Some(None),
                    };
                }
                Some(Value::Str {
                    value: out,
                    lang: common.flatten(),
                })
            }
            "SAMETERM" => {
                let a = self.eval_term(args.first()?, b, l)?;
                let c = self.eval_term(args.get(1)?, b, l)?;
                Some(Value::Bool(same_term(&a, &c)))
            }
            "COALESCE" => args.iter().find_map(|a| self.eval_value(a, b, l)),
            "REGEX" => {
                let text = self.eval_value(args.first()?, b, l)?.lexical();
                let pat = self.eval_value(args.get(1)?, b, l)?.lexical();
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
                // `as i64` saturates (NaN ⇒ 0), and the sum saturates too, so
                // huge positions such as SUBSTR(?s, 9e18, 9e18) cannot overflow.
                let end = match args.get(2) {
                    Some(e) => {
                        start.saturating_add(self.eval_value(e, b, l)?.as_f64()?.round() as i64)
                    }
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
                // Upstream gStore's SIMILARITY (TempResult.cpp:1278-1288):
                // `doSimilarity(x.term_value, y.term_value)`, its Jaro variant,
                // over the arguments' *term strings* — `<iri>`, `"lit"`,
                // `"lit"@en`, `"1"^^<…#integer>` (the dictionary key form that
                // upstream's `getStringByID` / constant `getText()` yield), so
                // the quotes and brackets take part in the comparison. The
                // legacy gstore-rust node/string similarity is `rs:SIMILARITY`.
                let av = self.eval_value(args.first()?, b, l)?;
                let cv = self.eval_value(args.get(1)?, b, l)?;
                Some(Value::Double(
                    crate::analytics::gfun::metrics::jaro_similarity(
                        &value_to_term(&av).dict_key(),
                        &value_to_term(&cv).dict_key(),
                    ),
                ))
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
            "CONTAINALL" | "CONTAINANY" => self.eval_contain(name == "CONTAINANY", args, b, l),
            "IF" => {
                if self.eval_ebv(args.first()?, b, l)? {
                    self.eval_value(args.get(1)?, b, l)
                } else {
                    self.eval_value(args.get(2)?, b, l)
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

            // ---- extended graph-analytics built-ins ----------------------
            // Every node-valued argument resolves to a dictionary entity id;
            // scalar results are xsd-typed literals (Int/Double/Bool) and
            // set/path results are space-serialised term strings (a bound "" when
            // empty), matching the SHORTESTPATHLEN return convention.

            // SSSP(a, b): shortest directed path a⇝b as space-joined terms;
            // "" when unreachable.
            "SSSP" => {
                let a = self.node_id(args.first()?, b, l)?;
                let c = self.node_id(args.get(1)?, b, l)?;
                let path = self.graph_view().shortest_path(a, c).unwrap_or_default();
                Some(Value::Str {
                    value: self.serialize_nodes(&path),
                    lang: None,
                })
            }
            // SSSPLEN(a, b): alias of SHORTESTPATHLEN — #edges, -1 if unreachable.
            "SSSPLEN" => {
                let a = self.node_id(args.first()?, b, l)?;
                let c = self.node_id(args.get(1)?, b, l)?;
                let len = self.graph_view().shortest_path_len(a, c);
                Some(Value::Int(len.map(|x| x as i64).unwrap_or(-1)))
            }
            // SHORTESTPATHS(a, b): number of distinct shortest paths a⇝b.
            "SHORTESTPATHS" => {
                let a = self.node_id(args.first()?, b, l)?;
                let c = self.node_id(args.get(1)?, b, l)?;
                let n = self.graph_view().count_shortest_paths(a, c)?;
                // The number of distinct shortest paths grows combinatorially and
                // can exceed i64::MAX; saturate rather than wrap to a negative.
                Some(Value::Int(n.min(i64::MAX as u64) as i64))
            }
            // KHOPCOUNT(a, k): #nodes at exactly k out-hops of a.
            "KHOPCOUNT" => {
                let a = self.node_id(args.first()?, b, l)?;
                let k = self.hop_arg(args.get(1)?, b, l)?;
                Some(Value::Int(self.graph_view().khop_count(a, k) as i64))
            }
            // KHOPNEIGHBOR(a, k): nodes at exactly k out-hops, serialised.
            "KHOPNEIGHBOR" => {
                let a = self.node_id(args.first()?, b, l)?;
                let k = self.hop_arg(args.get(1)?, b, l)?;
                let ids = self.graph_view().khop_neighbors(a, k);
                Some(Value::Str {
                    value: self.serialize_nodes(&ids),
                    lang: None,
                })
            }
            // KHOPALLNEIGHBORS(a, k): nodes within ≤ k out-hops (excl. a).
            "KHOPALLNEIGHBORS" => {
                let a = self.node_id(args.first()?, b, l)?;
                let k = self.hop_arg(args.get(1)?, b, l)?;
                let ids = self.graph_view().khop_all_neighbors(a, k);
                Some(Value::Str {
                    value: self.serialize_nodes(&ids),
                    lang: None,
                })
            }
            // KHOPENUMERATEPATH(a, k): simple paths of ≤ k edges from a; paths
            // separated by " ; ", nodes by " " (bounded to 1024 paths).
            "KHOPENUMERATEPATH" => {
                let a = self.node_id(args.first()?, b, l)?;
                let k = self.hop_arg(args.get(1)?, b, l)?;
                let paths = self.graph_view().khop_enumerate_paths(a, k, 1024);
                let s = paths
                    .iter()
                    .map(|p| self.serialize_nodes(p))
                    .collect::<Vec<_>>()
                    .join(" ; ");
                Some(Value::Str {
                    value: s,
                    lang: None,
                })
            }
            // BFSCOUNT(a [, k]): #nodes within ≤ k out-hops (excl. a); no k or
            // k < 0 ⇒ all reachable.
            "BFSCOUNT" => {
                let a = self.node_id(args.first()?, b, l)?;
                let k = match args.get(1) {
                    Some(e) => self.hop_arg(e, b, l)?,
                    None => u32::MAX,
                };
                Some(Value::Int(self.graph_view().bfs_count(a, k) as i64))
            }
            // WCC(a, b) ⇒ same weakly-connected component (bool);
            // WCC(a) ⇒ size of a's weakly-connected component (int).
            "WCC" => {
                let a = self.node_id(args.first()?, b, l)?;
                let comp = self.wcc_cached();
                match args.get(1) {
                    Some(e) => {
                        let c = self.node_id(e, b, l)?;
                        Some(Value::Bool(matches!(
                            (comp.get(&a), comp.get(&c)),
                            (Some(x), Some(y)) if x == y
                        )))
                    }
                    None => {
                        let rep = comp.get(&a)?;
                        Some(Value::Int(
                            comp.values().filter(|&r| r == rep).count() as i64
                        ))
                    }
                }
            }
            // LOUVAIN(a, b) ⇒ same Louvain community (bool);
            // LOUVAIN(a) ⇒ size of a's community (int).
            "LOUVAIN" => {
                let a = self.node_id(args.first()?, b, l)?;
                let comm = self.louvain_cached();
                match args.get(1) {
                    Some(e) => {
                        let c = self.node_id(e, b, l)?;
                        Some(Value::Bool(matches!(
                            (comm.get(&a), comm.get(&c)),
                            (Some(x), Some(y)) if x == y
                        )))
                    }
                    None => {
                        let cid = comm.get(&a)?;
                        Some(Value::Int(
                            comm.values().filter(|&x| x == cid).count() as i64
                        ))
                    }
                }
            }
            // LABELPROP(a, b) ⇒ same label-propagation community (bool);
            // LABELPROP(a) ⇒ size of a's community (int).
            "LABELPROP" => {
                let a = self.node_id(args.first()?, b, l)?;
                let comm = self.labelprop_cached();
                match args.get(1) {
                    Some(e) => {
                        let c = self.node_id(e, b, l)?;
                        Some(Value::Bool(matches!(
                            (comm.get(&a), comm.get(&c)),
                            (Some(x), Some(y)) if x == y
                        )))
                    }
                    None => {
                        let cid = comm.get(&a)?;
                        Some(Value::Int(
                            comm.values().filter(|&x| x == cid).count() as i64
                        ))
                    }
                }
            }
            // BETWEENNESSCENTRALITY(a): Brandes betweenness of node a.
            "BETWEENNESSCENTRALITY" => {
                let a = self.node_id(args.first()?, b, l)?;
                let bc = self.betweenness_cached();
                Some(Value::Double(bc.get(&a).copied().unwrap_or(0.0)))
            }
            // CLOSENESSCENTRALITY(a): Wasserman–Faust closeness of node a.
            "CLOSENESSCENTRALITY" => {
                let a = self.node_id(args.first()?, b, l)?;
                let cc = self.closeness_cached();
                Some(Value::Double(cc.get(&a).copied().unwrap_or(0.0)))
            }
            // TRIANGLECOUNTING(): global triangle count;
            // TRIANGLECOUNTING(a): triangles through node a.
            "TRIANGLECOUNTING" => match args.first() {
                None => Some(Value::Int(self.triangle_count_cached() as i64)),
                Some(e) => {
                    let a = self.node_id(e, b, l)?;
                    Some(Value::Int(
                        self.graph_view().node_triangle_count(a).unwrap_or(0) as i64,
                    ))
                }
            },
            // CLUSTERINGCOEFF(): global transitivity;
            // CLUSTERINGCOEFF(a): local clustering coefficient of node a.
            "CLUSTERINGCOEFF" => match args.first() {
                None => Some(Value::Double(self.global_clustering_cached())),
                Some(e) => {
                    let a = self.node_id(e, b, l)?;
                    Some(Value::Double(
                        self.graph_view()
                            .local_clustering_coefficient(a)
                            .unwrap_or(0.0),
                    ))
                }
            },
            // PPR(src, target): personalized-PageRank score of `target` when the
            // random walk restarts at `src`.
            "PPR" => {
                let src = self.node_id(args.first()?, b, l)?;
                let target = self.node_id(args.get(1)?, b, l)?;
                let ppr = self.ppr_cached(src);
                Some(Value::Double(ppr.get(&target).copied().unwrap_or(0.0)))
            }
            // JACCARDSIMILARITY(a, b): Jaccard index of the two neighbour sets.
            "JACCARDSIMILARITY" => {
                let a = self.node_id(args.first()?, b, l)?;
                let c = self.node_id(args.get(1)?, b, l)?;
                Some(Value::Double(
                    self.graph_view().jaccard_similarity(a, c).unwrap_or(0.0),
                ))
            }
            // DEGREECORRELATION(): Newman degree assortativity of the graph.
            "DEGREECORRELATION" => Some(Value::Double(self.degree_assortativity_cached())),
            // DIAMETERESTIMATION(): double-sweep BFS diameter estimate.
            "DIAMETERESTIMATION" => Some(Value::Int(self.diameter_cached() as i64)),

            // Every name in `builtins::BUILTINS` has an arm above (pinned by
            // the `every_table_builtin_has_an_engine_arm` test); this arm only
            // guards against the table and the dispatcher drifting apart.
            _ => None,
        }
    }

    /// gStore's `CONTAINALL(items, list [, sep])` / `CONTAINANY(items, list
    /// [, sep])` (upstream `TempResult::doComp`). A *list* is a literal typed
    /// `xsd:list` (`<http://www.w3.org/2001/XMLSchema#list>`) whose lexical
    /// form holds items separated by `sep` (default `","`); items are trimmed
    /// of spaces, and an empty last item is dropped (upstream `Util::split`).
    /// `items` is such a list, or any other term standing for the one item
    /// that is its lexical form (an IRI's IRI string). `list` must be a list.
    ///
    /// CONTAINALL is true iff every item of `items` occurs in `list`;
    /// CONTAINANY iff some item of `list` occurs in `items`. Items compare as
    /// strings. The result is an error (unbound) when `list` is not an
    /// `xsd:list`, or `sep` is not a non-empty simple string.
    ///
    /// Divergences from upstream, whose behaviour here is accidental:
    /// upstream only honours a one-character separator *literal* (any other
    /// third argument silently means `","`), and turns a non-list `items`
    /// term into an item by cutting the first and last character off its
    /// N-Triples form, which mangles language-tagged and typed literals
    /// (`"5"^^xsd:integer` becomes `5"^^<…#integer`); here the separator may
    /// be any non-empty string expression and the item is the lexical form.
    fn eval_contain(&self, any: bool, args: &[Expr], b: &Binding, l: &VarLayout) -> Option<Value> {
        let sep = match args.get(2) {
            None => ",".to_string(),
            Some(e) => match self.eval_value(e, b, l)? {
                Value::Str { value, lang: None } if !value.is_empty() => value,
                _ => return None,
            },
        };
        let first = self.eval_value(args.first()?, b, l)?;
        let items = match xsd_list_text(&first) {
            Some(text) => split_xsd_list(text, &sep),
            None => vec![first.lexical()],
        };
        let list_value = self.eval_value(args.get(1)?, b, l)?;
        let list = split_xsd_list(xsd_list_text(&list_value)?, &sep);
        Some(Value::Bool(if any {
            list.iter().any(|y| items.contains(y))
        } else {
            items.iter().all(|x| list.contains(x))
        }))
    }

    /// User-defined function fallback (gStore `pfnQuery`): a name that is not a
    /// SPARQL built-in is looked up in the registry before erroring. Arguments
    /// are evaluated to values first; if any errors/unbound the call yields
    /// `None`, like a built-in with a bad argument.
    fn eval_custom_function(
        &self,
        name: &str,
        args: &[Expr],
        b: &Binding,
        l: &VarLayout,
    ) -> Option<Value> {
        let f = self.functions.get(name)?;
        let mut argv = Vec::with_capacity(args.len());
        for a in args {
            argv.push(self.eval_value(a, b, l)?);
        }
        f(&argv)
    }

    /// A user function called through its IRI: registered under the full IRI,
    /// else under the IRI's local name (`ex:double(…)` reaches a function
    /// registered as `"double"`).
    fn eval_iri_function(
        &self,
        iri: &str,
        args: &[Expr],
        b: &Binding,
        l: &VarLayout,
    ) -> Option<Value> {
        let f = self.user_iri_function(iri)?;
        let mut argv = Vec::with_capacity(args.len());
        for a in args {
            argv.push(self.eval_value(a, b, l)?);
        }
        f(&argv)
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

    // ---- memoized whole-graph analytics (computed once per query) ------------
    // Each computes its full-graph pass on first use and caches the result, so a
    // graph built-in inside a per-row FILTER/projection doesn't rerun it per row.

    fn wcc_cached(&self) -> Rc<HashMap<u32, u32>> {
        if let Some(r) = self.algo_cache.borrow().wcc.clone() {
            return r;
        }
        let r = Rc::new(self.graph_view().weakly_connected_components().0);
        self.algo_cache.borrow_mut().wcc = Some(Rc::clone(&r));
        r
    }
    fn louvain_cached(&self) -> Rc<HashMap<u32, usize>> {
        if let Some(r) = self.algo_cache.borrow().louvain.clone() {
            return r;
        }
        let r = Rc::new(self.graph_view().louvain());
        self.algo_cache.borrow_mut().louvain = Some(Rc::clone(&r));
        r
    }
    fn labelprop_cached(&self) -> Rc<HashMap<u32, usize>> {
        if let Some(r) = self.algo_cache.borrow().labelprop.clone() {
            return r;
        }
        let r = Rc::new(self.graph_view().label_propagation());
        self.algo_cache.borrow_mut().labelprop = Some(Rc::clone(&r));
        r
    }
    fn betweenness_cached(&self) -> Rc<HashMap<u32, f64>> {
        if let Some(r) = self.algo_cache.borrow().betweenness.clone() {
            return r;
        }
        let r = Rc::new(self.graph_view().betweenness_centrality());
        self.algo_cache.borrow_mut().betweenness = Some(Rc::clone(&r));
        r
    }
    fn closeness_cached(&self) -> Rc<HashMap<u32, f64>> {
        if let Some(r) = self.algo_cache.borrow().closeness.clone() {
            return r;
        }
        let r = Rc::new(self.graph_view().closeness_centrality());
        self.algo_cache.borrow_mut().closeness = Some(Rc::clone(&r));
        r
    }
    fn ppr_cached(&self, src: u32) -> Rc<HashMap<u32, f64>> {
        if let Some(r) = self.algo_cache.borrow().ppr.get(&src).cloned() {
            return r;
        }
        let r = Rc::new(
            self.graph_view()
                .personalized_pagerank(&[src], 0.85, 100, 1e-9),
        );
        self.algo_cache.borrow_mut().ppr.insert(src, Rc::clone(&r));
        r
    }
    fn degree_assortativity_cached(&self) -> f64 {
        if let Some(v) = self.algo_cache.borrow().degree_assortativity {
            return v;
        }
        let v = self.graph_view().degree_assortativity();
        self.algo_cache.borrow_mut().degree_assortativity = Some(v);
        v
    }
    fn diameter_cached(&self) -> u32 {
        if let Some(v) = self.algo_cache.borrow().diameter {
            return v;
        }
        let v = self.graph_view().diameter_estimate();
        self.algo_cache.borrow_mut().diameter = Some(v);
        v
    }
    fn global_clustering_cached(&self) -> f64 {
        if let Some(v) = self.algo_cache.borrow().global_clustering {
            return v;
        }
        let v = self.graph_view().global_clustering_coefficient();
        self.algo_cache.borrow_mut().global_clustering = Some(v);
        v
    }
    fn triangle_count_cached(&self) -> u64 {
        if let Some(v) = self.algo_cache.borrow().triangle_count {
            return v;
        }
        let v = self.graph_view().triangle_count();
        self.algo_cache.borrow_mut().triangle_count = Some(v);
        v
    }

    /// Render a graph node's entity id back to its dictionary term string
    /// (`<iri>` / `"literal"`) for serialised set/path built-in results.
    fn node_label(&self, id: u32) -> String {
        self.resolve_string(Some(id), false).unwrap_or_default()
    }

    /// Serialise an entity-id list as space-separated term strings.
    fn serialize_nodes(&self, ids: &[u32]) -> String {
        ids.iter()
            .map(|&id| self.node_label(id))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Evaluate a SPARQL numeric argument into a hop budget: a negative value
    /// (e.g. `-1`) means "unbounded" (`u32::MAX`), mirroring `KHOPREACHABLE`.
    fn hop_arg(&self, e: &Expr, b: &Binding, l: &VarLayout) -> Option<u32> {
        let kf = self.eval_value(e, b, l)?.as_f64()?;
        Some(if kf < 0.0 { u32::MAX } else { kf as u32 })
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

    /// `UCASE` / `LCASE`: map the lexical form, keeping a language tag
    /// (§17.4.3.5-6: `UCASE("foo"@en)` is `"FOO"@en`).
    fn str1(
        &self,
        args: &[Expr],
        b: &Binding,
        l: &VarLayout,
        f: impl Fn(&str) -> String,
    ) -> Option<Value> {
        let (s, lang) = str_and_lang(&self.eval_value(args.first()?, b, l)?);
        Some(Value::Str { value: f(&s), lang })
    }

    /// `STRBEFORE` / `STRAFTER` (§17.4.3.7-8): the part of the first
    /// argument before / after the first occurrence of the second, with the
    /// first argument's language tag; no occurrence gives the empty simple
    /// literal.
    fn str_split(&self, args: &[Expr], b: &Binding, l: &VarLayout, before: bool) -> Option<Value> {
        let (a, lang) = str_and_lang(&self.eval_value(args.first()?, b, l)?);
        let c = self.eval_value(args.get(1)?, b, l)?.lexical();
        Some(match a.split_once(&c) {
            Some((head, tail)) => Value::Str {
                value: if before { head } else { tail }.to_string(),
                lang,
            },
            None => Value::Str {
                value: String::new(),
                lang: None,
            },
        })
    }

    fn str2_bool(
        &self,
        args: &[Expr],
        b: &Binding,
        l: &VarLayout,
        f: impl Fn(String, String) -> bool,
    ) -> Option<Value> {
        let a = self.eval_value(args.first()?, b, l)?.lexical();
        let c = self.eval_value(args.get(1)?, b, l)?.lexical();
        Some(Value::Bool(f(a, c)))
    }

    // ---- ordering ---------------------------------------------------------

    fn sort_solutions(
        &self,
        solutions: &mut [Binding],
        order_by: &[OrderCondition],
        layout: &VarLayout,
    ) {
        // Evaluate every sort key exactly once per solution, then sort on the
        // cached keys. Re-evaluating inside the comparator would be O(n log n)
        // evaluations and, for a non-deterministic key such as `ORDER BY
        // RAND()`, an inconsistent comparator — which `slice::sort_by` is
        // allowed to (and since Rust 1.81 may) answer with a panic.
        let keys: Vec<Vec<_>> = solutions
            .iter()
            .map(|s| {
                order_by
                    .iter()
                    .map(|cond| order_key(&self.eval_value(&cond.expr, s, layout)))
                    .collect()
            })
            .collect();
        let mut perm: Vec<usize> = (0..solutions.len()).collect();
        perm.sort_by(|&a, &b| {
            for (k, cond) in order_by.iter().enumerate() {
                let ord = keys[a][k].cmp(&keys[b][k]);
                let ord = if cond.descending { ord.reverse() } else { ord };
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            std::cmp::Ordering::Equal
        });
        let sorted: Vec<Binding> = perm
            .iter()
            .map(|&i| std::mem::take(&mut solutions[i]))
            .collect();
        for (slot, b) in solutions.iter_mut().zip(sorted) {
            *slot = b;
        }
    }
}

/// Materialize a runtime [`Value`] as an RDF [`Term`].
fn value_to_term(v: &Value) -> Term {
    match v {
        Value::Iri(s) => Term::Iri(s.clone()),
        Value::Blank(s) => Term::Blank(s.clone()),
        Value::Int(i) => Term::typed_literal(i.to_string(), xsd::INTEGER),
        Value::BigInt(_) => Term::typed_literal(v.lexical(), xsd::INTEGER),
        Value::Decimal(_) => Term::typed_literal(v.lexical(), xsd::DECIMAL),
        Value::Float(_) => Term::typed_literal(v.lexical(), XSD_FLOAT),
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

/// `xsd:date`, produced by the `xsd:date(…)` cast.
const XSD_DATE: &str = "http://www.w3.org/2001/XMLSchema#date";

/// An integer from an `xsd:integer` lexical (`i64`, else `i128`).
fn integer_value(lexical: &str) -> Option<Value> {
    match Value::from_term(&Term::typed_literal(lexical, xsd::INTEGER)) {
        v @ (Value::Int(_) | Value::BigInt(_)) => Some(v),
        _ => None,
    }
}

/// A decimal from an `xsd:decimal` lexical (`None` beyond its `i128` range).
fn decimal_value(lexical: &str) -> Option<Value> {
    match Value::from_term(&Term::typed_literal(lexical, xsd::DECIMAL)) {
        v @ Value::Decimal(_) => Some(v),
        _ => None,
    }
}

/// The integer part of a finite double (`None` for NaN / ±INF or beyond `i128`).
fn integer_from_f64(x: f64) -> Option<Value> {
    if !x.is_finite() {
        return None;
    }
    let t = x.trunc();
    if t.abs() < 9.0e18 {
        return Some(Value::Int(t as i64));
    }
    if t.abs() < 1.7e38 {
        return Some(Value::BigInt(t as i128));
    }
    None
}

/// What an XSD cast reads its argument as (SPARQL 1.1 §17.5, the rows of the
/// casting table).
enum CastSource<'v> {
    Iri(&'v str),
    Str(&'v str),
    Number(&'v Value),
    Float(f64),
    Bool(bool),
    DateTime(&'v str),
    Date(&'v str),
    /// A literal of another datatype: castable to `xsd:string` only.
    Other(&'v str),
}

/// The SPARQL 1.1 XSD constructor functions (§17.5): `xsd:string(?x)`,
/// `xsd:integer(?x)`, `xsd:decimal`, `xsd:double`, `xsd:float`,
/// `xsd:boolean`, `xsd:dateTime`, plus `xsd:date` (XPath casting rules).
/// `None` (an expression error: FILTER false, BIND/projection unbound) for a
/// cast the table forbids — blank nodes, language-tagged strings, IRIs to
/// anything but `xsd:string`, ill-typed literals, NaN/INF to an integer or
/// decimal, … — and for a string whose lexical form is invalid for the
/// target type.
fn xsd_cast(target: XsdCast, v: &Value) -> Option<Value> {
    // An ill-typed literal ("abc"^^xsd:integer, "300"^^xsd:byte,
    // "2020-13-01"^^xsd:date) has no value to cast.
    if v.is_ill_typed() {
        return None;
    }
    let src = match v {
        Value::Iri(s) => CastSource::Iri(s),
        Value::Blank(_) => return None,
        Value::Str { lang: Some(_), .. } => return None,
        Value::Str { value, lang: None } => CastSource::Str(value),
        Value::Int(_) | Value::BigInt(_) | Value::Decimal(_) | Value::Double(_) => {
            CastSource::Number(v)
        }
        Value::Float(x) => CastSource::Float(*x),
        Value::Bool(b) => CastSource::Bool(*b),
        Value::Typed { value, datatype } => match datatype.as_str() {
            XSD_DATETIME => CastSource::DateTime(value),
            XSD_DATE => CastSource::Date(value),
            _ => CastSource::Other(value),
        },
    };
    let string = |s: String| Value::Str {
        value: s,
        lang: None,
    };
    match target {
        XsdCast::String => Some(match src {
            CastSource::Iri(s) | CastSource::Str(s) | CastSource::Other(s) => string(s.into()),
            CastSource::DateTime(s) | CastSource::Date(s) => string(s.into()),
            CastSource::Number(n) => string(n.lexical()),
            CastSource::Float(x) => string(xsd_float_lexical(x)),
            CastSource::Bool(b) => string(b.to_string()),
        }),
        XsdCast::Integer => match src {
            CastSource::Str(s) => {
                let s = s.trim();
                is_integer_lexical(s).then(|| integer_value(s)).flatten()
            }
            CastSource::Number(n @ (Value::Int(_) | Value::BigInt(_))) => Some(n.clone()),
            CastSource::Number(Value::Decimal(d)) => {
                // Truncate toward zero: the integer part of the lexical form.
                let lex = Value::Decimal(*d).lexical();
                let int = lex.split('.').next().unwrap_or("0");
                integer_value(if int == "-0" || int == "-" { "0" } else { int })
            }
            CastSource::Number(n) => integer_from_f64(n.as_f64()?),
            CastSource::Float(x) => integer_from_f64(x),
            CastSource::Bool(b) => Some(Value::Int(b as i64)),
            _ => None,
        },
        XsdCast::Decimal => match src {
            CastSource::Str(s) => {
                let s = s.trim();
                is_decimal_lexical(s).then(|| decimal_value(s)).flatten()
            }
            CastSource::Number(Value::Decimal(d)) => Some(Value::Decimal(*d)),
            CastSource::Number(n @ (Value::Int(_) | Value::BigInt(_))) => {
                decimal_value(&n.lexical())
            }
            CastSource::Number(n) => {
                let x = n.as_f64()?;
                // Rust prints a finite f64 positionally (never with an
                // exponent), the shortest digits that round-trip.
                x.is_finite()
                    .then(|| decimal_value(&x.to_string()))
                    .flatten()
            }
            CastSource::Float(x) => {
                let f = x as f32;
                f.is_finite()
                    .then(|| decimal_value(&f.to_string()))
                    .flatten()
            }
            CastSource::Bool(b) => decimal_value(if b { "1.0" } else { "0.0" }),
            _ => None,
        },
        XsdCast::Double | XsdCast::Float => {
            let x = match src {
                CastSource::Str(s) => parse_xsd_double(s)?,
                CastSource::Number(n) => n.as_f64()?,
                CastSource::Float(x) => x,
                CastSource::Bool(b) => f64::from(u8::from(b)),
                _ => return None,
            };
            Some(match target {
                XsdCast::Float => Value::Float(to_f32_precision(x)),
                _ => Value::Double(x),
            })
        }
        XsdCast::Boolean => match src {
            CastSource::Str(s) => match s.trim() {
                "true" | "1" => Some(Value::Bool(true)),
                "false" | "0" => Some(Value::Bool(false)),
                _ => None,
            },
            CastSource::Number(n) => {
                let x = n.as_f64()?;
                Some(Value::Bool(x != 0.0 && !x.is_nan()))
            }
            CastSource::Float(x) => Some(Value::Bool(x != 0.0 && !x.is_nan())),
            CastSource::Bool(b) => Some(Value::Bool(b)),
            _ => None,
        },
        XsdCast::DateTime => {
            let lexical = match src {
                CastSource::Str(s) | CastSource::DateTime(s) => {
                    let s = s.trim();
                    (s.contains('T') && parse_datetime_parts(s).is_some()).then(|| s.to_string())?
                }
                // xs:date → xs:dateTime: midnight, same timezone (XPath).
                CastSource::Date(s) => {
                    let s = s.trim();
                    if s.contains('T') || parse_datetime_parts(s).is_none() {
                        return None;
                    }
                    let (date, tz) = split_date_tz(s);
                    format!("{date}T00:00:00{tz}")
                }
                _ => return None,
            };
            Some(Value::Typed {
                value: lexical,
                datatype: XSD_DATETIME.into(),
            })
        }
        XsdCast::Date => {
            let lexical = match src {
                CastSource::Str(s) | CastSource::Date(s) => {
                    let s = s.trim();
                    (!s.contains('T') && parse_datetime_parts(s).is_some())
                        .then(|| s.to_string())?
                }
                // xs:dateTime → xs:date: the date part, same timezone.
                CastSource::DateTime(s) => {
                    let s = s.trim();
                    parse_datetime_parts(s)?;
                    let (date_time, tz) = split_date_tz(s);
                    let date = date_time.split('T').next()?;
                    format!("{date}{tz}")
                }
                _ => return None,
            };
            Some(Value::Typed {
                value: lexical,
                datatype: XSD_DATE.into(),
            })
        }
    }
}

/// Split a valid `xsd:date` / `xsd:dateTime` lexical into the part before
/// its timezone and the timezone (`Z`, `±hh:mm`, or empty).
fn split_date_tz(s: &str) -> (&str, &str) {
    if let Some(body) = s.strip_suffix('Z') {
        return (body, "Z");
    }
    // A `±hh:mm` zone is the last six characters. Without a zone those end
    // in `-MM-DD` (a date: `:` missing) or `mm:ss` / fractional digits (a
    // dateTime: no leading sign), so the shape test cannot misfire.
    if let Some(at) = s.len().checked_sub(6).filter(|&at| s.is_char_boundary(at)) {
        let (body, tz) = s.split_at(at);
        if tz.starts_with(['+', '-']) && tz.as_bytes()[3] == b':' {
            return (body, tz);
        }
    }
    (s, "")
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
        let unreserved = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~');
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
        h(bytes[0]),
        h(bytes[1]),
        h(bytes[2]),
        h(bytes[3]),
        h(bytes[4]),
        h(bytes[5]),
        h(bytes[6]),
        h(bytes[7]),
        h(bytes[8]),
        h(bytes[9]),
        h(bytes[10]),
        h(bytes[11]),
        h(bytes[12]),
        h(bytes[13]),
        h(bytes[14]),
        h(bytes[15]),
    )
}

/// Jaro string similarity in `[0, 1]` over Unicode scalar values, with
/// `TempResult::doSimilarity`'s equal/empty/substring short-cuts but textbook
/// transposition counting, in `f64`: the string mode of the legacy
/// `rs:SIMILARITY`. The bare `SIMILARITY` uses the exact upstream port,
/// [`crate::analytics::gfun::metrics::jaro_similarity`].
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

/// gStore's list datatype, used by `CONTAINALL` / `CONTAINANY`.
const XSD_LIST: &str = "http://www.w3.org/2001/XMLSchema#list";

/// The lexical form of an `xsd:list` literal, if `v` is one.
fn xsd_list_text(v: &Value) -> Option<&str> {
    match v {
        Value::Typed { value, datatype } if datatype == XSD_LIST => Some(value),
        _ => None,
    }
}

/// Split an `xsd:list` lexical form as upstream's `Util::split` does: items
/// are separated by `sep` and trimmed of spaces; an empty last item (so also
/// the only item of an empty list) is dropped, empty inner items are kept.
fn split_xsd_list(text: &str, sep: &str) -> Vec<String> {
    let mut items: Vec<String> = text
        .split(sep)
        .map(|t| t.trim_matches(' ').to_string())
        .collect();
    if items.last().is_some_and(String::is_empty) {
        items.pop();
    }
    items
}

/// `lhs IN (list…)` / `lhs NOT IN (list…)` (SPARQL 1.1 §17.4.1.9-10), with
/// `eval` evaluating an operand (`None` = error/unbound). The test is `=`
/// ([`Value::sparql_eq`]: value equality where the operator mapping defines
/// it, else RDFterm-equal, so `2 IN (<http://ex/iri>, 2.0)` is true and
/// `2 IN ("2")` a type error) on each list member in turn: IN is true as soon as one member is equal; an
/// error in some comparison makes the result an error unless a later or
/// earlier member is equal (`2 IN (1/0, 2)` is true, `2 IN (3, 1/0)` an
/// error); otherwise it is false. NOT IN is the negation under the same error
/// rule. An empty list gives false (true for NOT IN) whatever the left-hand
/// side, as the spec's expansion into a `||`/`&&` chain of zero comparisons
/// does; otherwise an error in the left-hand side is an error. Members are
/// evaluated lazily, left to right, stopping at the first equal one.
fn eval_in(
    lhs: &Expr,
    list: &[Expr],
    negated: bool,
    eval: impl Fn(&Expr) -> Option<Value>,
) -> Option<Value> {
    if list.is_empty() {
        return Some(Value::Bool(negated));
    }
    let x = eval(lhs)?;
    let mut error = false;
    for e in list {
        match eval(e).and_then(|y| x.sparql_eq(&y)) {
            Some(true) => return Some(Value::Bool(!negated)),
            Some(false) => {}
            None => error = true,
        }
    }
    if error {
        None
    } else {
        Some(Value::Bool(negated))
    }
}

/// Sum numeric values (integer-preserving until a double appears).
fn sum_values(vals: &[Value]) -> Value {
    // Accumulate through the numeric tower so decimals and big integers are
    // summed exactly (not silently skipped) and any double promotes the result.
    // Non-numeric values are not summable and are skipped (as before).
    let mut acc = Value::Int(0);
    for v in vals {
        if v.is_numeric() {
            if let Some(next) = acc.arith(ArithOp::Add, v) {
                acc = next;
            }
        }
    }
    acc
}

/// How a variable of a trailing `VALUES` block meets an aggregate query's
/// group rows ([`Evaluator::eval_aggregation`]).
#[derive(Debug, Clone, Copy)]
enum ValuesCol {
    /// A GROUP BY or projected variable: the VALUES term must equal the
    /// row's (an unbound column takes it).
    Match(usize),
    /// A SELECT expression alias: matched like `Match`, once the expressions
    /// are computed.
    Alias(usize),
    /// Out of scope after grouping: the VALUES term binds the column.
    Bind(usize),
}

/// The hidden group-row variable holding the `i`-th lifted aggregate of an
/// aggregate query (`.` cannot occur in a SPARQL variable name, so it never
/// clashes with a query variable).
fn agg_var(i: usize) -> String {
    format!(".agg{i}")
}

/// SPARQL 1.1 §18.2.4.1: `e` with every aggregate replaced by the variable
/// of its group-row column ([`agg_var`]); new aggregates are appended to
/// `aggs` (an aggregate already there — the same call written twice — reuses
/// its column). An aggregate's own argument is left as is.
fn lift_aggregates(e: &Expr, aggs: &mut Vec<Expr>) -> Expr {
    let mut lift = |x: &Expr| lift_aggregates(x, aggs);
    match e {
        Expr::Aggregate { .. } => {
            let i = match aggs.iter().position(|a| a == e) {
                Some(i) => i,
                None => {
                    aggs.push(e.clone());
                    aggs.len() - 1
                }
            };
            Expr::Var(agg_var(i))
        }
        Expr::Or(a, b) => Expr::Or(Box::new(lift(a)), Box::new(lift(b))),
        Expr::And(a, b) => Expr::And(Box::new(lift(a)), Box::new(lift(b))),
        Expr::Compare(op, a, b) => Expr::Compare(*op, Box::new(lift(a)), Box::new(lift(b))),
        Expr::Arith(op, a, b) => Expr::Arith(*op, Box::new(lift(a)), Box::new(lift(b))),
        Expr::Not(a) => Expr::Not(Box::new(lift(a))),
        Expr::Unary(op, a) => Expr::Unary(*op, Box::new(lift(a))),
        Expr::Builtin(name, args) => Expr::Builtin(name.clone(), args.iter().map(lift).collect()),
        Expr::IriCall { iri, args } => Expr::IriCall {
            iri: iri.clone(),
            args: args.iter().map(lift).collect(),
        },
        Expr::In {
            expr,
            list,
            negated,
        } => Expr::In {
            expr: Box::new(lift(expr)),
            list: list.iter().map(lift).collect(),
            negated: *negated,
        },
        Expr::Var(_)
        | Expr::Const(_)
        | Expr::Exists(_, _)
        | Expr::GraphFn { .. }
        | Expr::Pfn { .. } => e.clone(),
    }
}

/// The layout of an aggregate query's group rows: the WHERE `layout` plus
/// one hidden column per lifted aggregate ([`agg_var`]).
fn group_layout(layout: &VarLayout, n_aggs: usize) -> VarLayout {
    let mut names = layout.names.clone();
    let mut is_pred = layout.is_pred.clone();
    for i in 0..n_aggs {
        names.push(agg_var(i));
        is_pred.push(false);
    }
    VarLayout::from_vars(names, is_pred)
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
        Expr::Builtin(_, args) | Expr::IriCall { args, .. } => args.iter().any(expr_has_aggregate),
        Expr::In { expr, list, .. } => {
            expr_has_aggregate(expr) || list.iter().any(expr_has_aggregate)
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// advanced graph functions: projection shape
// ---------------------------------------------------------------------------

/// A function only evaluated as a whole SELECT projection: an upstream
/// advanced graph function, or a `PFN.name({…})` call, which upstream
/// evaluates the same way (one row per the shape rules, its arguments
/// gathered over all solutions — see the module docs).
enum ProjCall {
    Graph(GCall),
    Pfn { name: String, json: String },
}

impl ProjCall {
    /// The call's name, for messages.
    fn label(&self) -> String {
        match self {
            ProjCall::Graph(c) => c.kind.name().to_string(),
            ProjCall::Pfn { name, .. } => format!("PFN.{name}"),
        }
    }
}

/// A SELECT projection item `(f(…) AS ?alias)` whose expression is an
/// upstream advanced graph function or a PFN call.
struct GraphFnItem {
    call: ProjCall,
    alias: String,
}

/// The advanced graph functions and PFN calls projected by `q`, in
/// projection order.
fn graph_fn_items(q: &SelectQuery) -> Result<Vec<GraphFnItem>> {
    let Projection::Items(items) = &q.projection else {
        return Ok(Vec::new());
    };
    items
        .iter()
        .filter_map(|it| match it {
            SelectItem::Expr(Expr::GraphFn { func, args }, alias) => {
                Some(GCall::from_ast(*func, args).map(|call| GraphFnItem {
                    call: ProjCall::Graph(call),
                    alias: alias.clone(),
                }))
            }
            SelectItem::Expr(Expr::Pfn { name, json }, alias) => Some(Ok(GraphFnItem {
                call: ProjCall::Pfn {
                    name: name.clone(),
                    json: json.clone(),
                },
                alias: alias.clone(),
            })),
            _ => None,
        })
        .collect()
}

/// The size of upstream's projection list for `q`: the projected items plus
/// every ORDER BY variable that is not projected (upstream appends those to
/// `proj`, GE:1418-1437, and the row shape of a graph function depends on
/// whether `proj` has a single entry).
fn upstream_projection_len(q: &SelectQuery) -> usize {
    let projected = q.result_vars();
    let mut extra: Vec<String> = Vec::new();
    for o in &q.order_by {
        let mut vars = Vec::new();
        expr_vars(&o.expr, &mut vars);
        for v in vars {
            if !projected.contains(&v) && !extra.contains(&v) {
                extra.push(v);
            }
        }
    }
    projected.len() + extra.len()
}

/// The variables an expression mentions (not those of nested patterns).
fn expr_vars(e: &Expr, out: &mut Vec<String>) {
    match e {
        Expr::Var(v) => out.push(v.clone()),
        Expr::Const(_) | Expr::Exists(_, _) | Expr::Pfn { .. } => {}
        Expr::Or(a, b) | Expr::And(a, b) | Expr::Compare(_, a, b) | Expr::Arith(_, a, b) => {
            expr_vars(a, out);
            expr_vars(b, out);
        }
        Expr::Not(a) | Expr::Unary(_, a) => expr_vars(a, out),
        Expr::Builtin(_, args) | Expr::IriCall { args, .. } => {
            args.iter().for_each(|a| expr_vars(a, out))
        }
        Expr::In { expr, list, .. } => {
            expr_vars(expr, out);
            list.iter().for_each(|a| expr_vars(a, out));
        }
        Expr::Aggregate { arg, .. } => {
            if let Some(a) = arg {
                expr_vars(a, out);
            }
        }
        Expr::GraphFn { args, .. } => {
            for a in args {
                if let GArg::Node(VarOrIri::Var(v)) = a {
                    out.push(v.clone());
                }
            }
        }
    }
}

/// `WHERE {}`: upstream evaluates it to no rows at all (its result table is
/// created empty, GE:1390-1394), where SPARQL has one empty solution.
fn pattern_is_trivially_empty(gp: &GraphPattern) -> bool {
    match gp {
        GraphPattern::Empty => true,
        GraphPattern::Bgp(tps) => tps.is_empty(),
        _ => false,
    }
}

/// Upstream's `singleBGP` (QueryParser.cpp:1033-1034): the WHERE clause is
/// triple patterns only — no FILTER, OPTIONAL, UNION, BIND, VALUES, MINUS,
/// property path, … — which enables its LIMIT pushdown. (The parser folds
/// a bare nested group `{ { … } }` of triple patterns into one BGP, so that
/// counts here too; upstream would not push its LIMIT down.)
fn pattern_is_single_bgp(gp: &GraphPattern) -> bool {
    matches!(gp, GraphPattern::Bgp(tps) if !tps.is_empty())
}

/// The variables `gp` binds only through `BIND` (no triple, path, VALUES,
/// sub-SELECT or `GRAPH ?g` binds them too). Upstream always stores a BIND
/// result in a string column (`TempResult::doBind`, TempResult.cpp:1566),
/// which a graph function rejects as a vertex, whether or not the value is
/// a stored term.
fn bind_only_vars(gp: &GraphPattern) -> HashSet<String> {
    fn walk(gp: &GraphPattern, binds: &mut Vec<String>, other: &mut Vec<String>) {
        match gp {
            GraphPattern::Extend(inner, v, _) => {
                walk(inner, binds, other);
                binds.push(v.clone());
            }
            GraphPattern::Join(a, b)
            | GraphPattern::Union(a, b)
            | GraphPattern::Minus(a, b)
            | GraphPattern::LeftJoin(a, b, _) => {
                walk(a, binds, other);
                walk(b, binds, other);
            }
            GraphPattern::Filter(_, inner) | GraphPattern::Service { pattern: inner, .. } => {
                walk(inner, binds, other)
            }
            GraphPattern::Graph(g, inner) => {
                if let GraphTerm::Var(v) = g {
                    other.push(v.clone());
                }
                walk(inner, binds, other);
            }
            GraphPattern::Empty
            | GraphPattern::Bgp(_)
            | GraphPattern::Path(_)
            | GraphPattern::Values(_, _)
            | GraphPattern::SubSelect(_) => gp.collect_vars(other),
        }
    }
    let (mut binds, mut other) = (Vec::new(), Vec::new());
    walk(gp, &mut binds, &mut other);
    binds.into_iter().filter(|v| !other.contains(v)).collect()
}

// ---------------------------------------------------------------------------
// call validation (before evaluation)
// ---------------------------------------------------------------------------

/// The `xsd:` namespace of the SPARQL 1.1 constructor (cast) functions.
const XSD_NS: &str = "http://www.w3.org/2001/XMLSchema#";

/// The target of an XSD constructor function call, `xsd:integer(?x)` & co.
/// (SPARQL 1.1 §17.5, plus `xsd:date`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum XsdCast {
    String,
    Integer,
    Decimal,
    Double,
    Float,
    Boolean,
    DateTime,
    Date,
}

impl XsdCast {
    /// The cast an IRI function call names, if any (XSD local names are
    /// case-sensitive).
    fn from_iri(iri: &str) -> Option<XsdCast> {
        Some(match iri.strip_prefix(XSD_NS)? {
            "string" => XsdCast::String,
            "integer" => XsdCast::Integer,
            "decimal" => XsdCast::Decimal,
            "double" => XsdCast::Double,
            "float" => XsdCast::Float,
            "boolean" => XsdCast::Boolean,
            "dateTime" => XsdCast::DateTime,
            "date" => XsdCast::Date,
            _ => return None,
        })
    }
}

/// The local name of a function IRI (after the last `#`, `/` or `:`), for
/// looking up user functions registered under a bare name.
fn iri_local_name(iri: &str) -> &str {
    iri.rsplit(['#', '/', ':']).next().unwrap_or(iri)
}

impl<'a, S: TripleSource> Evaluator<'a, S> {
    /// Reject, before evaluation, the calls this engine cannot evaluate, so
    /// the query fails with a clear message instead of silently yielding
    /// unbound values:
    ///
    /// * an upstream advanced graph function anywhere but as the whole
    ///   expression of a SELECT projection (upstream gStore parses
    ///   `simpleCycleBoolean`/`cycleBoolean`/`shortestPathLen`/`kHopReachable`
    ///   inside FILTER — QueryParser.cpp:866-930 — but `TempResult::doComp`
    ///   has no case for them, so they never evaluate there);
    /// * an advanced function whose arguments do not fit its signature (the
    ///   parser never produces one, but an AST may be built by hand);
    /// * `PFN.name({…})` (personalised functions arrive in a later phase);
    /// * an IRI function call that is neither an XSD cast, nor a legacy `rs:`
    ///   function, nor a user function registered under its IRI or its local
    ///   name.
    fn check_calls(&self, q: &Query) -> Result<()> {
        match q {
            Query::Select(s) => self.check_select_calls(s),
            Query::Ask(a) => self.check_pattern_calls(&a.pattern),
            Query::Construct(c) => {
                self.check_pattern_calls(&c.pattern)?;
                c.order_by
                    .iter()
                    .try_for_each(|o| self.check_expr_calls(&o.expr))
            }
            Query::Describe(d) => match &d.pattern {
                Some(p) => self.check_pattern_calls(p),
                None => Ok(()),
            },
            Query::Update(ops) => self.check_update_calls(ops),
        }
    }

    /// The same checks for every `WHERE` clause of an UPDATE request. They
    /// only read the request and the function registry, so
    /// [`Database`](crate::Database) runs them before the first operation:
    /// a statically invalid request is rejected as a whole, with nothing
    /// applied.
    pub fn check_update_calls(&self, ops: &[UpdateOp]) -> Result<()> {
        ops.iter().try_for_each(|op| match op {
            UpdateOp::Modify { pattern, .. } => self.check_pattern_calls(pattern),
            _ => Ok(()),
        })
    }

    fn check_select_calls(&self, s: &SelectQuery) -> Result<()> {
        if let Projection::Items(items) = &s.projection {
            for it in items {
                match it {
                    // The one place an advanced function evaluates.
                    SelectItem::Expr(Expr::GraphFn { func, args }, _) => {
                        GCall::from_ast(*func, args)?;
                    }
                    // So does a PFN call; its argument must be a JSON object.
                    SelectItem::Expr(Expr::Pfn { name, json }, _) => {
                        crate::pfn::call::parse_args(name, json)?;
                    }
                    SelectItem::Expr(e, _) => self.check_expr_calls(e)?,
                    SelectItem::Var(_) => {}
                }
            }
        }
        self.check_pattern_calls(&s.pattern)?;
        for e in s.group_by.iter().chain(&s.having) {
            self.check_expr_calls(e)?;
        }
        s.order_by
            .iter()
            .try_for_each(|o| self.check_expr_calls(&o.expr))
    }

    fn check_pattern_calls(&self, gp: &GraphPattern) -> Result<()> {
        match gp {
            GraphPattern::Empty
            | GraphPattern::Bgp(_)
            | GraphPattern::Path(_)
            | GraphPattern::Values(_, _) => Ok(()),
            GraphPattern::Join(a, b) | GraphPattern::Union(a, b) | GraphPattern::Minus(a, b) => {
                self.check_pattern_calls(a)?;
                self.check_pattern_calls(b)
            }
            GraphPattern::LeftJoin(a, b, fs) => {
                self.check_pattern_calls(a)?;
                self.check_pattern_calls(b)?;
                fs.iter().try_for_each(|e| self.check_expr_calls(e))
            }
            GraphPattern::Filter(fs, inner) => {
                fs.iter().try_for_each(|e| self.check_expr_calls(e))?;
                self.check_pattern_calls(inner)
            }
            GraphPattern::Extend(inner, _, e) => {
                self.check_pattern_calls(inner)?;
                self.check_expr_calls(e)
            }
            GraphPattern::SubSelect(sq) => self.check_select_calls(sq),
            GraphPattern::Graph(_, inner) => self.check_pattern_calls(inner),
            GraphPattern::Service { pattern, .. } => self.check_pattern_calls(pattern),
        }
    }

    fn check_expr_calls(&self, e: &Expr) -> Result<()> {
        match e {
            Expr::GraphFn { func, .. } => {
                let name = func.name();
                let mut msg = format!(
                    "{name}(…) is only evaluated as the whole expression of a SELECT \
                     projection, e.g. SELECT ({name}(…) AS ?v); upstream gStore parses it \
                     inside FILTER but never evaluates it there"
                );
                if rs_function_name(&format!("{RS_NAMESPACE}{}", func.keyword())).is_some() {
                    msg.push_str(&format!(
                        "; the legacy gstore-rust function rs:{}(…) works anywhere",
                        func.keyword()
                    ));
                }
                Err(GStoreError::Query(msg))
            }
            Expr::Pfn { name, .. } => Err(GStoreError::Query(format!(
                "PFN.{name}(…) is only evaluated as the whole expression of a SELECT \
                 projection, e.g. SELECT (PFN.{name}({{…}}) AS ?v)"
            ))),
            Expr::IriCall { iri, args } => {
                self.check_iri_call(iri, args.len())?;
                args.iter().try_for_each(|a| self.check_expr_calls(a))
            }
            Expr::Var(_) | Expr::Const(_) => Ok(()),
            Expr::Or(a, b) | Expr::And(a, b) | Expr::Compare(_, a, b) | Expr::Arith(_, a, b) => {
                self.check_expr_calls(a)?;
                self.check_expr_calls(b)
            }
            Expr::Not(a) | Expr::Unary(_, a) => self.check_expr_calls(a),
            Expr::Builtin(_, args) => args.iter().try_for_each(|a| self.check_expr_calls(a)),
            Expr::In { expr, list, .. } => {
                self.check_expr_calls(expr)?;
                list.iter().try_for_each(|a| self.check_expr_calls(a))
            }
            Expr::Exists(_, p) => self.check_pattern_calls(p),
            Expr::Aggregate { arg, .. } => match arg {
                Some(a) => self.check_expr_calls(a),
                None => Ok(()),
            },
        }
    }

    /// An IRI function call must name an `rs:` legacy function (the parser
    /// checked name and arity), a one-argument XSD cast, or a registered user
    /// function (by full IRI, else by local name).
    fn check_iri_call(&self, iri: &str, nargs: usize) -> Result<()> {
        if rs_function_name(iri).is_some() {
            return Ok(());
        }
        if XsdCast::from_iri(iri).is_some() {
            if nargs != 1 {
                return Err(GStoreError::Query(format!(
                    "the cast <{iri}>(…) takes exactly one argument, got {nargs}"
                )));
            }
            return Ok(());
        }
        if self.user_iri_function(iri).is_some() {
            return Ok(());
        }
        Err(GStoreError::Query(format!(
            "unknown function <{iri}>: it is not an XSD cast (xsd:string, xsd:integer, \
             xsd:decimal, xsd:double, xsd:float, xsd:boolean, xsd:dateTime, xsd:date), not a \
             legacy gstore-rust function (<{RS_NAMESPACE}NAME>, rs:NAME), and no user function \
             is registered under this IRI or its local name \"{}\"",
            iri_local_name(iri)
        )))
    }

    /// The user function an IRI call reaches: registered under the full IRI,
    /// else under its local name.
    fn user_iri_function(&self, iri: &str) -> Option<&super::functions::CustomFn> {
        self.functions
            .get(iri)
            .or_else(|| self.functions.get(iri_local_name(iri)))
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
        .filter(|l| !minus_removes(l, &right, &shared))
        .collect()
}

/// Whether MINUS drops `l`: some right solution is compatible with it on the
/// `shared` variables and they share at least one bound one.
fn minus_removes(l: &Binding, right: &[Binding], shared: &[usize]) -> bool {
    right.iter().any(|r| {
        let mut compatible = true;
        let mut any_shared = false;
        for &v in shared {
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
}

/// DISTINCT, OFFSET and LIMIT over a SELECT's projected rows, in that order.
fn apply_row_modifiers(q: &SelectQuery, mut rows: Vec<Vec<Option<u32>>>) -> Vec<Vec<Option<u32>>> {
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
    rows
}

/// The order in which [`Evaluator::eval_join`] joins its conjuncts, given
/// each one's `(solution count, variables)`: the smallest first, then
/// repeatedly the smallest one connected to what is joined so far (any
/// smallest one when none is connected); ties go to the conjunct the
/// `swap_remove`-based selection meets first. The streaming join (`topk`)
/// uses the same order, so both produce the solutions in the same order.
fn greedy_join_order(parts: &[(usize, &[usize])]) -> Vec<usize> {
    let mut rest: Vec<usize> = (0..parts.len()).collect();
    let mut order = Vec::with_capacity(parts.len());
    let Some(start) = (0..rest.len()).min_by_key(|&i| parts[rest[i]].0) else {
        return order;
    };
    let first = rest.swap_remove(start);
    let mut acc_vars: Vec<usize> = parts[first].1.to_vec();
    order.push(first);
    while !rest.is_empty() {
        let pick = (0..rest.len())
            .min_by_key(|&i| {
                let (len, vars) = parts[rest[i]];
                let connected = vars.iter().any(|v| acc_vars.contains(v));
                (!connected as u8, len)
            })
            .unwrap_or(0);
        let next = rest.swap_remove(pick);
        for &v in parts[next].1 {
            if !acc_vars.contains(&v) {
                acc_vars.push(v);
            }
        }
        order.push(next);
    }
    order
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

/// Per-BGP translation caches of a mixed pipeline (both directions):
/// entity-space id → predicate id, predicate id → entity-space id
/// ([`Evaluator::mixed_enter`] / [`Evaluator::mixed_leave`]).
#[derive(Default)]
struct MixedCaches {
    to_pred: HashMap<u32, Option<u32>>,
    to_node: HashMap<u32, Option<u32>>,
}

/// The `(x, hidden)` slot pairs of the mixed variables whose hidden
/// predicate slot some pattern of a compiled BGP binds
/// ([`VarLayout::pred_alias`]); non-empty ⇒ the BGP runs as a mixed
/// pipeline. Sorted, so every executor translates in the same order.
fn mixed_slots(plans: &[PatPlan], layout: &VarLayout) -> Vec<(usize, usize)> {
    let mut mixed: Vec<(usize, usize)> = layout
        .pred_alias
        .iter()
        .filter(|&(_, &h)| plans.iter().any(|p| matches!(p.p, Slot::Var(v) if v == h)))
        .map(|(&x, &h)| (x, h))
        .collect();
    mixed.sort_unstable();
    mixed
}

/// Clear the hidden predicate slots of a finished mixed-pipeline row, so it
/// carries each mixed variable in its own (entity-space) slot only.
fn mixed_clear(b: &mut Binding, mixed: &[(usize, usize)]) {
    for &(_, h) in mixed {
        b[h] = None;
    }
}

/// The variables `gp` binds other than in a triple's predicate position:
/// subjects and objects of triple and path patterns, BIND targets, VALUES
/// columns, sub-SELECT results and GRAPH / SERVICE variables — the ids such
/// a binding carries are entity/literal (or synthetic) ids. A variable both
/// here and in predicate position is *mixed* ([`VarLayout::pred_alias`]).
fn node_bound_vars(gp: &GraphPattern, out: &mut HashSet<String>) {
    let mut add = |t: &PatternTerm| {
        if let PatternTerm::Var(v) = t {
            out.insert(v.clone());
        }
    };
    match gp {
        GraphPattern::Empty => {}
        GraphPattern::Bgp(tps) => {
            for tp in tps {
                add(&tp.subject);
                add(&tp.object);
            }
        }
        GraphPattern::Path(p) => {
            add(&p.subject);
            add(&p.object);
        }
        GraphPattern::Join(a, b)
        | GraphPattern::Union(a, b)
        | GraphPattern::Minus(a, b)
        | GraphPattern::LeftJoin(a, b, _) => {
            node_bound_vars(a, out);
            node_bound_vars(b, out);
        }
        GraphPattern::Filter(_, inner) => node_bound_vars(inner, out),
        GraphPattern::Extend(inner, v, _) => {
            node_bound_vars(inner, out);
            out.insert(v.clone());
        }
        GraphPattern::Values(_, _) | GraphPattern::SubSelect(_) => {
            let mut vars = Vec::new();
            gp.collect_vars(&mut vars);
            out.extend(vars);
        }
        GraphPattern::Graph(g, inner) => {
            if let GraphTerm::Var(v) = g {
                out.insert(v.clone());
            }
            node_bound_vars(inner, out);
        }
        GraphPattern::Service {
            endpoint, pattern, ..
        } => {
            if let ServiceRef::Var(v) = endpoint {
                out.insert(v.clone());
            }
            node_bound_vars(pattern, out);
        }
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

/// A fast, deterministic, std-only hasher for the hash join's key table: the
/// FxHash mixing step (rotate, xor, multiply) over each written word. The
/// join keys are small tuples of ids, for which SipHash's per-call setup and
/// rounds dominate. Not DoS-resistant, which is fine for keys made of the
/// database's own ids. The join's output order never depends on the hasher
/// (see [`join_sets_hash`]).
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct FxHasher {
    hash: u64,
}

/// The FxHash multiplier.
const FX_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(FX_SEED);
    }
}

impl std::hash::Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut word = [0u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            self.add(u64::from_le_bytes(word));
        }
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(u64::from(i));
    }
    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.add(u64::from(i));
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(u64::from(i));
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

/// The [`BuildHasher`](std::hash::BuildHasher) of [`FxHasher`].
pub(crate) type FxBuildHasher = std::hash::BuildHasherDefault<FxHasher>;

/// Hash join on the shared-variable key (gStore `multi_join`): build a hash
/// table on the right side, then probe it once per left row.
///
/// The output is left-major, right-minor — each left row in order, joined with
/// its matching right rows in their original order — whatever the hasher.
fn join_sets_hash(left: &[Binding], right: &[Binding], shared: &[usize]) -> Vec<Binding> {
    join_sets_hash_with::<FxBuildHasher>(left, right, shared)
}

/// [`join_sets_hash`] with the key table hashed by `H`.
fn join_sets_hash_with<H: std::hash::BuildHasher + Default>(
    left: &[Binding],
    right: &[Binding],
    shared: &[usize],
) -> Vec<Binding> {
    let key_of = |b: &Binding| -> Vec<Option<u32>> { shared.iter().map(|&i| b[i]).collect() };
    let mut index: HashMap<Vec<Option<u32>>, Vec<&Binding>, H> = HashMap::default();
    for r in right {
        index.entry(key_of(r)).or_default().push(r);
    }
    let mut out = Vec::new();
    // One probe key buffer, reused for every left row.
    let mut key: Vec<Option<u32>> = Vec::with_capacity(shared.len());
    for l in left {
        key.clear();
        key.extend(shared.iter().map(|&i| l[i]));
        if let Some(matches) = index.get(key.as_slice()) {
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

/// The candidate set of a variable slot, if it has one.
fn slot_candidates<'c>(slot: &Slot, cands: &'c Candidates) -> Option<&'c [u32]> {
    match slot {
        Slot::Var(v) => cands.get(v).map(Vec::as_slice),
        Slot::Const(_) => None,
    }
}

/// Whether `value` passes a (sorted) candidate set; `None` admits anything.
/// `cand_ok` with the set looked up beforehand.
fn in_candidates(set: Option<&[u32]>, value: u32) -> bool {
    set.is_none_or(|list| list.binary_search(&value).is_ok())
}

/// A predicate scan is replaced by candidate lookups when the candidates
/// number less than `1 / CANDIDATE_LOOKUP_FACTOR` of the predicate's triples.
const CANDIDATE_LOOKUP_FACTOR: usize = 8;

/// Is `value` an allowed binding for `slot` under the VS-tree candidate sets?
/// Constants and variables without a candidate set are always allowed. The
/// executor uses [`slot_candidates`] + [`in_candidates`]; this per-triple
/// lookup is the tests' reference filter.
#[cfg(test)]
fn cand_ok(slot: &Slot, value: u32, cands: &Candidates) -> bool {
    match slot {
        // Candidate lists are sorted; membership is a binary search.
        Slot::Var(v) => cands
            .get(v)
            .is_none_or(|list| list.binary_search(&value).is_ok()),
        Slot::Const(_) => true,
    }
}

/// Minimum dictionary size (entities + literals) below which the substring index
/// is not built — a linear scan beats indexing for a tiny dictionary.
const STRING_INDEX_MIN_ENTRIES: usize = 64;

/// Walk a `FILTER` expression collecting mandatory-substring constraints for
/// StringIndex pruning (see [`Evaluator::filter_string_constraints`]). Recurses
/// through top-level `AND`; a disjunction (`OR`) is not mandatory, so skipped.
fn collect_substr_constraints(e: &Expr, layout: &VarLayout, out: &mut HashMap<usize, Vec<String>>) {
    match e {
        Expr::And(a, b) => {
            collect_substr_constraints(a, layout, out);
            collect_substr_constraints(b, layout, out);
        }
        Expr::Builtin(name, args) => {
            // Only the exact two-argument shapes qualify; matching on the slice
            // pattern (rather than indexing `args[0]`) keeps a zero-argument
            // call such as `FILTER(STR())` from panicking here.
            let (target, needle) = match (name.as_str(), args.as_slice()) {
                ("CONTAINS" | "STRSTARTS" | "STRENDS", [target, pat]) => {
                    (target, const_string(pat))
                }
                // A metacharacter-free regex pattern is just a literal substring.
                // Only the plain two-arg form: a flags arg (e.g. "i") could break
                // byte-substring soundness.
                ("REGEX", [target, pat]) => (
                    target,
                    const_string(pat).filter(|p| is_plain_literal_regex(p)),
                ),
                _ => return,
            };
            if let (Some(vi), Some(n)) = (substr_target_var(target, layout), needle) {
                if n.len() >= 3 {
                    out.entry(vi).or_default().push(n);
                }
            }
        }
        _ => {}
    }
}

/// The variable index a substring builtin's first argument denotes, if it is a
/// bare variable or `STR(?v)` (both preserve the value's bytes). Case-folding
/// wrappers (`LCASE`/`UCASE`) yield `None`: their substring is not the variable's
/// own bytes, so index pruning would be unsound.
fn substr_target_var(e: &Expr, layout: &VarLayout) -> Option<usize> {
    match e {
        Expr::Var(v) => layout.index.get(v).copied(),
        Expr::Builtin(name, args) if name == "STR" => match args.as_slice() {
            [inner] => substr_target_var(inner, layout),
            _ => None,
        },
        _ => None,
    }
}

/// The lexical value of a constant expression, for use as a substring needle.
fn const_string(e: &Expr) -> Option<String> {
    match e {
        Expr::Const(t) => Some(Value::from_term(t).lexical()),
        _ => None,
    }
}

/// Whether `p` is a plain literal (no regex metacharacters), so it can be treated
/// as a mandatory substring for candidate pruning.
fn is_plain_literal_regex(p: &str) -> bool {
    !p.is_empty() && !p.bytes().any(|b| b".^$*+?()[]{}|\\".contains(&b))
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

/// `rdf:langString`, the datatype of a language-tagged literal.
const RDF_LANG_STRING: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#langString";

/// RDF term equality, for `sameTerm`: a simple literal and the
/// `xsd:string` literal of the same text are one term (RDF 1.1).
fn same_term(a: &Term, b: &Term) -> bool {
    match (a, b) {
        (
            Term::Literal {
                value: va,
                datatype: da,
                lang: la,
            },
            Term::Literal {
                value: vb,
                datatype: db,
                lang: lb,
            },
        ) => {
            fn dt(d: &Option<String>) -> Option<&str> {
                d.as_deref().filter(|d| *d != xsd::STRING)
            }
            va == vb && la == lb && dt(da) == dt(db)
        }
        _ => a == b,
    }
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
        // A hidden blank-node variable goes out as a blank node, which the
        // remote reads as a non-distinguished variable.
        PatternTerm::Var(v) if is_anon_var(v) => {
            format!("_:anon{}", &v[ANON_VAR_PREFIX.len()..])
        }
        PatternTerm::Var(v) => format!("?{v}"),
        PatternTerm::Term(t) => t.to_string(),
    }
}

#[cfg(test)]
mod builtin_tests {
    use super::*;
    use crate::model::IdTriple;
    use crate::parser::sparql::parse;

    #[test]
    fn sum_values_includes_decimal_and_bigint() {
        // #6: SUM must fold decimals and big integers through the numeric tower,
        // not silently skip them (which returned 0 for an all-decimal group).
        let decimals = vec![
            Value::from_term(&Term::typed_literal("1.5", xsd::DECIMAL)),
            Value::from_term(&Term::typed_literal("2.25", xsd::DECIMAL)),
        ];
        assert_eq!(sum_values(&decimals).lexical(), "3.75");
        // Mixed int/bigint stays exact.
        assert_eq!(
            sum_values(&[Value::BigInt(5), Value::Int(2)]).lexical(),
            "7"
        );
        // Empty group is still 0.
        assert_eq!(sum_values(&[]), Value::Int(0));
    }

    #[test]
    fn substring_filter_constraints_are_sound() {
        // #4: only filters whose truth *requires* a fixed ≥3-byte substring of a
        // subject/object variable's value are extracted (sound supersets); every
        // other shape is ignored so the exact FILTER remains the source of truth.
        let layout = VarLayout {
            index: HashMap::from([("s".to_string(), 0usize), ("o".to_string(), 1usize)]),
            names: vec!["s".into(), "o".into()],
            is_pred: vec![false, false],
            pred_alias: HashMap::new(),
        };
        let lit = |s: &str| Expr::Const(Term::plain_literal(s));

        // CONTAINS(?o, "marker") → constraint on ?o (index 1).
        let mut out = HashMap::new();
        collect_substr_constraints(
            &Expr::Builtin(
                "CONTAINS".into(),
                vec![Expr::Var("o".into()), lit("marker")],
            ),
            &layout,
            &mut out,
        );
        assert_eq!(out.get(&1), Some(&vec!["marker".to_string()]));

        // STR(?s) wrapper + STRSTARTS, and a metacharacter-free REGEX, both extract.
        let mut out2 = HashMap::new();
        collect_substr_constraints(
            &Expr::And(
                Box::new(Expr::Builtin(
                    "STRSTARTS".into(),
                    vec![
                        Expr::Builtin("STR".into(), vec![Expr::Var("s".into())]),
                        lit("http://ex/"),
                    ],
                )),
                Box::new(Expr::Builtin(
                    "REGEX".into(),
                    vec![Expr::Var("o".into()), lit("plain")],
                )),
            ),
            &layout,
            &mut out2,
        );
        assert_eq!(out2.get(&0), Some(&vec!["http://ex/".to_string()]));
        assert_eq!(out2.get(&1), Some(&vec!["plain".to_string()]));

        // Skipped: metachar REGEX, case-folding wrapper, <3B needle, disjunction.
        let mut out3 = HashMap::new();
        for e in [
            Expr::Builtin("REGEX".into(), vec![Expr::Var("o".into()), lit("a.c")]),
            Expr::Builtin(
                "CONTAINS".into(),
                vec![
                    Expr::Builtin("LCASE".into(), vec![Expr::Var("o".into())]),
                    lit("marker"),
                ],
            ),
            Expr::Builtin("CONTAINS".into(), vec![Expr::Var("o".into()), lit("ab")]),
            Expr::Or(
                Box::new(Expr::Builtin(
                    "CONTAINS".into(),
                    vec![Expr::Var("o".into()), lit("marker")],
                )),
                Box::new(Expr::Builtin(
                    "CONTAINS".into(),
                    vec![Expr::Var("s".into()), lit("other")],
                )),
            ),
        ] {
            collect_substr_constraints(&e, &layout, &mut out3);
        }
        assert!(
            out3.is_empty(),
            "unsound/unhelpful filters extract nothing: {out3:?}"
        );
    }

    /// Regression: `FILTER(STR())` reached `collect_substr_constraints`, which
    /// indexed `args[0]` of every built-in call and panicked on an empty list.
    #[test]
    fn substring_constraints_tolerate_zero_arg_calls() {
        let layout = VarLayout::from_vars(vec!["v".into()], vec![false]);
        let mut out = HashMap::new();
        for name in ["STR", "CONTAINS", "STRSTARTS", "STRENDS", "REGEX", "LCASE"] {
            collect_substr_constraints(&Expr::Builtin(name.into(), vec![]), &layout, &mut out);
            let nested = Expr::Builtin(
                "CONTAINS".into(),
                vec![
                    Expr::Builtin("STR".into(), vec![]),
                    Expr::Const(Term::plain_literal("abc")),
                ],
            );
            collect_substr_constraints(&nested, &layout, &mut out);
        }
        assert!(out.is_empty());
    }

    /// The engine's built-in dispatcher and the shared arity table
    /// (`parser::sparql::builtins::BUILTINS`) must name exactly the same
    /// functions: a table entry without an engine arm would silently evaluate to
    /// an error, and an arm missing from the table would be unreachable (its
    /// name would be routed to the user-function registry instead).
    #[test]
    fn every_table_builtin_has_an_engine_arm() {
        let src = include_str!("engine.rs");
        let start = src.find("fn eval_builtin(").expect("eval_builtin present");
        let end = src[start..]
            .find("fn eval_custom_function(")
            .map(|i| start + i)
            .expect("eval_custom_function follows eval_builtin");
        let body = &src[start..end];
        let re =
            Regex::new(r#""([A-Z][A-Z0-9_]*)"\s*(?:=>|\|)|name == "([A-Z][A-Z0-9_]*)""#).unwrap();
        let handled: HashSet<&str> = re
            .captures_iter(body)
            .filter_map(|c| c.get(1).or_else(|| c.get(2)).map(|m| m.as_str()))
            .collect();
        let table: HashSet<&str> = builtins::BUILTINS.iter().map(|b| b.name).collect();
        let missing_arm: Vec<_> = table.difference(&handled).collect();
        let missing_entry: Vec<_> = handled.difference(&table).collect();
        assert!(
            missing_arm.is_empty(),
            "table names without an engine arm: {missing_arm:?}"
        );
        assert!(
            missing_entry.is_empty(),
            "engine arms missing from the table: {missing_entry:?}"
        );
    }

    /// Every built-in evaluated with 0–4 arguments, as an AST the parser would
    /// have rejected, yields a value or an evaluation error — never a panic.
    #[test]
    fn builtins_with_any_arity_never_panic_in_the_evaluator() {
        let (d, s) = fixture(true);
        let ev = Evaluator::new(&d, &s);
        let layout = VarLayout::from_vars(vec!["n".into()], vec![false]);
        let name_id = d.term_id(&Term::plain_literal("Hello World")).unwrap();
        let binding: Binding = vec![Some(name_id)];
        let arg_pool = [
            Expr::Var("n".into()),
            Expr::Const(Term::iri("http://ex/a")),
            Expr::Const(Term::typed_literal("2", xsd::INTEGER)),
            Expr::Var("unbound_var".into()),
        ];
        for spec in builtins::BUILTINS {
            for n in 0..=4 {
                let args: Vec<Expr> = arg_pool.iter().cycle().take(n).cloned().collect();
                let e = Expr::Builtin(spec.name.to_string(), args);
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    ev.eval_value(&e, &binding, &layout)
                }));
                let v = r.unwrap_or_else(|_| panic!("{}({n} args) panicked", spec.name));
                if !spec.arity.accepts(n) {
                    assert!(
                        v.is_none(),
                        "{}({n} args) must be an evaluation error",
                        spec.name
                    );
                }
            }
        }
    }

    /// `ORDER BY RAND()` used to re-evaluate the key inside the comparator,
    /// handing `sort_by` an inconsistent order (which it may answer with a
    /// panic). Keys are now computed once per row.
    #[test]
    fn order_by_rand_is_stable_and_panic_free() {
        let mut d = Dictionary::new();
        let p = d.intern_predicate(&Term::iri("http://ex/p").dict_key());
        let o = d.intern_term(&Term::iri("http://ex/o"));
        let mut triples = Vec::new();
        for i in 0..400 {
            let s = d.intern_term(&Term::iri(format!("http://ex/s{i}")));
            triples.push(IdTriple::new(s, p, o));
        }
        let mut s = TripleStore::new();
        s.bulk_load(triples);
        for q in [
            "SELECT ?s WHERE { ?s <http://ex/p> ?o } ORDER BY (RAND())",
            "SELECT ?s WHERE { ?s <http://ex/p> ?o } ORDER BY DESC(RAND()) ?s",
            "SELECT ?s (RAND() AS ?r) WHERE { ?s <http://ex/p> ?o } ORDER BY ?r",
        ] {
            let parsed = parse(q).unwrap();
            for _ in 0..5 {
                match Evaluator::new(&d, &s).evaluate(&parsed).unwrap() {
                    QueryResult::Select(rs) => assert_eq!(rs.row_count(), 400, "{q}"),
                    other => panic!("expected SELECT, got {other:?}"),
                }
            }
        }
        // A deterministic key still sorts correctly through the cached keys.
        let parsed =
            parse("SELECT ?s WHERE { ?s <http://ex/p> ?o } ORDER BY DESC(?s) LIMIT 2").unwrap();
        let QueryResult::Select(rs) = Evaluator::new(&d, &s).evaluate(&parsed).unwrap() else {
            panic!("expected SELECT")
        };
        assert_eq!(
            rs.rows,
            vec![
                vec![Some("<http://ex/s99>".into())],
                vec![Some("<http://ex/s98>".into())]
            ]
        );
    }

    #[test]
    fn substr_with_huge_positions_does_not_overflow() {
        assert_eq!(scalar(r#"SUBSTR("abc", 9e18, 9e18)"#), r#""""#);
        assert_eq!(scalar(r#"SUBSTR("abc", -9e18, 9e18)"#), r#""""#);
        assert_eq!(scalar(r#"SUBSTR("abc", 2, 9e18)"#), r#""bc""#);
        assert_eq!(scalar(r#"SUBSTR("abc", 1e300, 1e300)"#), r#""""#);
    }

    #[test]
    fn datetime_with_huge_year_compares_without_overflow() {
        let huge =
            "\"999999999999999999-01-01T00:00:00Z\"^^<http://www.w3.org/2001/XMLSchema#dateTime>";
        let neg =
            "\"-9223372036854775807-01-01T00:00:00Z\"^^<http://www.w3.org/2001/XMLSchema#dateTime>";
        assert!(scalar(&format!("({huge} > {DTL})")).contains("true"));
        assert!(scalar(&format!("({neg} < {DTL})")).contains("true"));
        assert!(scalar(&format!("YEAR({huge})")).contains("999999999999999999"));
    }

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

    #[test]
    fn cancel_flag_aborts_read_with_cancelled_error() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let (d, s) = fixture(false);
        let q = parse("SELECT ?s ?p ?o WHERE { ?s ?p ?o }").unwrap();
        // A pre-set cancel flag aborts the read with Cancelled.
        let flag = Arc::new(AtomicBool::new(true));
        let err = Evaluator::new(&d, &s)
            .with_cancel(flag)
            .evaluate(&q)
            .unwrap_err();
        assert!(
            matches!(err, GStoreError::Cancelled),
            "expected Cancelled, got {err:?}"
        );
        // An unset flag runs to completion.
        let ok = Arc::new(AtomicBool::new(false));
        assert!(Evaluator::new(&d, &s).with_cancel(ok).evaluate(&q).is_ok());
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
        assert_eq!(
            scalar(r#"REPLACE("Hello", "[aeiou]", "_", "i")"#),
            r#""H_ll_""#
        );
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
        // Bare SIMILARITY is upstream's `doSimilarity` over the *term
        // strings*, so quotes and brackets take part in the comparison.
        let sim = |e: &str| -> f64 { lit(&scalar(e)).parse().unwrap() };
        let upstream = crate::analytics::gfun::metrics::jaro_similarity;
        assert_eq!(lit(&scalar(r#"SIMILARITY("abc", "abc")"#)), "1.0");
        // `"abc"` vs `"xyz"`: only the two quotes match: (2/5 + 2/5 + 2/2)/3.
        let v = sim(r#"SIMILARITY("abc", "xyz")"#);
        assert!((v - 0.6).abs() < 1e-6, "got {v}");
        // `"abc"` is no substring of `"abcdef"` (the closing quote differs):
        // four matches, no transposition: (4/8 + 4/5 + 4/4)/3.
        let v = sim(r#"SIMILARITY("abcdef", "abc")"#);
        assert!((v - 2.3 / 3.0).abs() < 1e-6, "got {v}");
        // Exactly the upstream port (f32 arithmetic, byte-wise), on term strings.
        assert_eq!(
            sim(r#"SIMILARITY("MARTHA", "MARHTA")"#),
            upstream("\"MARTHA\"", "\"MARHTA\"")
        );
        assert_eq!(
            sim("SIMILARITY(<http://ex/a>, <http://ex/c>)"),
            upstream("<http://ex/a>", "<http://ex/c>")
        );
        assert_eq!(
            sim(r#"SIMILARITY(?n, "Hello")"#),
            upstream("\"Hello World\"", "\"Hello\"")
        );
        assert_eq!(
            lit(&scalar("SIMILARITY(<http://ex/a>, <http://ex/a>)")),
            "1.0"
        );
        // The legacy gstore-rust SIMILARITY (lexical forms, textbook Jaro)
        // stays reachable as rs:SIMILARITY: "abc" is a substring of "abcdef"
        // there, (3/6 + 2)/3.
        let v = sim(r#"rs:SIMILARITY("abcdef", "abc")"#);
        assert!((v - 2.5 / 3.0).abs() < 1e-9, "got {v}");
        assert_eq!(lit(&scalar(r#"rs:SIMILARITY("abc", "xyz")"#)), "0.0");
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
        assert!(
            uuid.starts_with("<urn:uuid:") && uuid.ends_with('>'),
            "{uuid}"
        );
        let struuid = lit(&scalar("STRUUID()"));
        assert_eq!(struuid.len(), 36);
        assert_eq!(struuid.matches('-').count(), 4);
        // version-4 nibble.
        assert_eq!(&struuid[14..15], "4");
    }

    // ---- hashing ----------------------------------------------------------

    #[test]
    fn hash_builtins_match_known_vectors() {
        assert_eq!(
            lit(&scalar(r#"MD5("abc")"#)),
            "900150983cd24fb0d6963f7d28e17f72"
        );
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
            count(
                r#"<http://ex/a> <http://ex/name> ?n FILTER(STRSTARTS(SUBSTR(?n,1,5), "Hello"))"#
            ),
            1
        );
        assert_eq!(
            count(
                r#"<http://ex/a> <http://ex/name> ?n FILTER(LANGMATCHES("en", "*") && IF(1>0, true, false))"#
            ),
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
        assert_eq!(lit(&scalar_on(false, "rs:CYCLEBOOLEAN()")), "false");
        assert_eq!(lit(&scalar_on(true, "rs:CYCLEBOOLEAN()")), "true");
        assert_eq!(lit(&scalar_on(true, "rs:SIMPLECYCLEBOOLEAN()")), "true");
    }

    #[test]
    fn shortestpathlen_counts_edges() {
        assert_eq!(
            lit(&scalar("rs:SHORTESTPATHLEN(<http://ex/a>, <http://ex/d>)")),
            "3"
        );
        assert_eq!(
            lit(&scalar("rs:SHORTESTPATHLEN(<http://ex/a>, <http://ex/a>)")),
            "0"
        );
        // d→a is unreachable in the acyclic graph ⇒ -1 sentinel.
        assert_eq!(
            lit(&scalar("rs:SHORTESTPATHLEN(<http://ex/d>, <http://ex/a>)")),
            "-1"
        );
    }

    #[test]
    fn khopreachable_respects_budget() {
        assert_eq!(
            lit(&scalar("rs:KHOPREACHABLE(<http://ex/a>, <http://ex/d>, 3)")),
            "true"
        );
        assert_eq!(
            lit(&scalar("rs:KHOPREACHABLE(<http://ex/a>, <http://ex/d>, 2)")),
            "false"
        );
        assert_eq!(
            lit(&scalar("rs:KHOPREACHABLE(<http://ex/a>, <http://ex/a>, 0)")),
            "true"
        );
    }

    // ---- extended graph-analytics built-ins ------------------------------

    #[test]
    fn sssp_serializes_the_path() {
        assert_eq!(
            lit(&scalar("rs:SSSP(<http://ex/a>, <http://ex/d>)")),
            "<http://ex/a> <http://ex/b> <http://ex/c> <http://ex/d>"
        );
        // Unreachable ⇒ empty (but bound) string.
        assert_eq!(lit(&scalar("rs:SSSP(<http://ex/d>, <http://ex/a>)")), "");
    }

    #[test]
    fn sssplen_aliases_shortestpathlen() {
        assert_eq!(
            lit(&scalar("rs:SSSPLEN(<http://ex/a>, <http://ex/d>)")),
            "3"
        );
        assert_eq!(
            lit(&scalar("rs:SSSPLEN(<http://ex/d>, <http://ex/a>)")),
            "-1"
        );
    }

    #[test]
    fn shortestpaths_counts_distinct_paths() {
        assert_eq!(
            lit(&scalar("rs:SHORTESTPATHS(<http://ex/a>, <http://ex/d>)")),
            "1"
        );
        assert_eq!(
            lit(&scalar("rs:SHORTESTPATHS(<http://ex/a>, <http://ex/a>)")),
            "1"
        );
    }

    #[test]
    fn khop_count_and_neighbor_and_all() {
        // a's out-edges: b, "Hello World", <dt> ⇒ 3 nodes at exactly 1 hop.
        assert_eq!(lit(&scalar("rs:KHOPCOUNT(<http://ex/a>, 1)")), "3");
        // Exactly 2 hops from a is just {c}.
        assert_eq!(lit(&scalar("rs:KHOPCOUNT(<http://ex/a>, 2)")), "1");
        assert_eq!(
            lit(&scalar("rs:KHOPNEIGHBOR(<http://ex/a>, 2)")),
            "<http://ex/c>"
        );
        assert_eq!(
            lit(&scalar("rs:KHOPNEIGHBOR(<http://ex/b>, 1)")),
            "<http://ex/c>"
        );
    }

    #[test]
    fn bfscount_counts_reachable() {
        // All reachable from a (excl. a): b, c, d, "Hello World", <dt> ⇒ 5.
        assert_eq!(lit(&scalar("rs:BFSCOUNT(<http://ex/a>)")), "5");
        assert_eq!(lit(&scalar("rs:BFSCOUNT(<http://ex/a>, 1)")), "3");
        assert_eq!(lit(&scalar("rs:BFSCOUNT(<http://ex/a>, 2)")), "4");
    }

    #[test]
    fn khop_enumerate_paths_serializes() {
        // From b: b→c and b→c→d.
        assert_eq!(
            lit(&scalar("KHOPENUMERATEPATH(<http://ex/b>, 2)")),
            "<http://ex/b> <http://ex/c> ; <http://ex/b> <http://ex/c> <http://ex/d>"
        );
    }

    #[test]
    fn wcc_membership_and_size() {
        // Every node hangs off a ⇒ one weakly-connected component of 6 nodes.
        assert_eq!(lit(&scalar("rs:WCC(<http://ex/a>, <http://ex/d>)")), "true");
        assert_eq!(lit(&scalar("rs:WCC(<http://ex/a>)")), "6");
    }

    #[test]
    fn community_builtins_group_the_graph() {
        // The whole graph is one connected blob ⇒ same label-prop community.
        assert_eq!(
            lit(&scalar("rs:LABELPROP(<http://ex/a>, <http://ex/d>)")),
            "true"
        );
        // Louvain returns a community size (≥ 1) for a known node.
        let sz: usize = lit(&scalar("rs:LOUVAIN(<http://ex/a>)")).parse().unwrap();
        assert!(sz >= 1);
    }

    #[test]
    fn triangle_and_clustering_are_zero_on_a_tree() {
        assert_eq!(lit(&scalar("rs:TRIANGLECOUNTING()")), "0");
        assert_eq!(lit(&scalar("rs:TRIANGLECOUNTING(<http://ex/a>)")), "0");
        // Acyclic fan/path ⇒ no closed triples ⇒ transitivity 0.0.
        assert_eq!(lit(&scalar("CLUSTERINGCOEFF()")), "0.0");
        assert_eq!(lit(&scalar("CLUSTERINGCOEFF(<http://ex/a>)")), "0.0");
    }

    #[test]
    fn centrality_builtins_rank_intermediate_nodes() {
        // A source node is never an intermediary ⇒ betweenness 0.
        let bc_a: f64 = lit(&scalar("rs:BETWEENNESSCENTRALITY(<http://ex/a>)"))
            .parse()
            .unwrap();
        let bc_b: f64 = lit(&scalar("rs:BETWEENNESSCENTRALITY(<http://ex/b>)"))
            .parse()
            .unwrap();
        assert_eq!(bc_a, 0.0);
        assert!(bc_b > 0.0);
        // a reaches five nodes ⇒ positive closeness.
        let cc_a: f64 = lit(&scalar("rs:CLOSENESSCENTRALITY(<http://ex/a>)"))
            .parse()
            .unwrap();
        assert!(cc_a > 0.0);
    }

    #[test]
    fn jaccard_and_node_similarity() {
        // JACCARDSIMILARITY is a dedicated graph function (literal-as-node is fine):
        // N(b) = {a, c}, N("Hello World") = {a}: intersection {a}, union {a, c}.
        assert_eq!(
            lit(&scalar(
                r#"rs:JACCARDSIMILARITY(<http://ex/b>, "Hello World")"#
            )),
            "0.5"
        );
        // Node-mode SIMILARITY requires BOTH args to be IRIs naming graph nodes;
        // a node vs itself has an identical neighbour set ⇒ 1.0. (Node mode
        // is the legacy gstore-rust SIMILARITY, rs:SIMILARITY; upstream's bare
        // SIMILARITY is string similarity.)
        assert_eq!(
            lit(&scalar("rs:SIMILARITY(<http://ex/a>, <http://ex/a>)")),
            "1.0"
        );
        // Regression (#3): two STORED LITERALS must use STRING similarity, not
        // node cosine. Both are objects of <a>, so node mode would wrongly report
        // a cosine of 1.0; string (Jaro) similarity of these very different
        // lexical forms is low.
        let s: f64 = lit(&scalar(
            r#"SIMILARITY("Hello World", "2011-01-10T14:45:13.815-05:00"^^<http://www.w3.org/2001/XMLSchema#dateTime>)"#
        ))
        .parse()
        .unwrap();
        assert!(
            s < 0.7,
            "stored literals must use string similarity, got {s}"
        );
        // String-mode SIMILARITY still works for non-node arguments.
        assert_eq!(lit(&scalar(r#"SIMILARITY("abc", "abc")"#)), "1.0");
    }

    #[test]
    fn degree_correlation_and_diameter() {
        let r: f64 = lit(&scalar("rs:DEGREECORRELATION()")).parse().unwrap();
        assert!(r.is_finite() && (-1.0..=1.0).contains(&r));
        // Undirected longest shortest path: leaf → a → b → c → d = 4.
        assert_eq!(lit(&scalar("rs:DIAMETERESTIMATION()")), "4");
    }

    #[test]
    fn ppr_prefers_the_source() {
        let self_score: f64 = lit(&scalar("rs:PPR(<http://ex/a>, <http://ex/a>)"))
            .parse()
            .unwrap();
        let far_score: f64 = lit(&scalar("rs:PPR(<http://ex/a>, <http://ex/d>)"))
            .parse()
            .unwrap();
        assert!(self_score > far_score);
        assert!(far_score >= 0.0);
    }

    #[test]
    fn graph_builtin_inside_filter() {
        // Keep only rows where a can reach d within 3 hops (it can).
        assert_eq!(
            count("<http://ex/a> <http://ex/name> ?n FILTER(rs:SSSPLEN(<http://ex/a>, <http://ex/d>) = 3)"),
            1
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

    /// The hash join's output order does not depend on the hasher: the fast
    /// deterministic one, randomly keyed SipHash and the nested loop agree
    /// row for row, with repeated keys on both sides and unbound slots.
    #[test]
    fn candidate_lookups_equal_the_filtered_predicate_scan() {
        // `extend` answers a `?s p ?o` scan from small candidate sets by
        // lookups: the same solutions, in the same order, as scanning the
        // predicate and filtering by the candidates.
        let mut state = 0xC0FFEE_u64;
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
        for round in 0..60 {
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
            let binding: Binding = vec![None; 2];
            let mut fast = Vec::new();
            ev.extend(&binding, &plan, &cands, &mut fast);
            let mut slow = Vec::new();
            for (s, pr, o) in ev.candidate_triples(&binding, &plan) {
                if !cand_ok(&plan.s, s, &cands) || !cand_ok(&plan.o, o, &cands) {
                    continue;
                }
                let mut nb = binding.clone();
                if unify(&mut nb, &plan.s, s)
                    && unify(&mut nb, &plan.p, pr)
                    && unify(&mut nb, &plan.o, o)
                {
                    slow.push(nb);
                }
            }
            assert_eq!(fast, slow, "round {round}");
        }
    }

    #[test]
    fn hash_join_order_does_not_depend_on_the_hasher() {
        let mut state = 0x1234_5678_u64;
        let mut next = move |n: u32| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 33) % u64::from(n)) as u32
        };
        for shared in [vec![0usize], vec![0, 1]] {
            let mut row = |n: u32| -> Binding {
                let mut r = vec![None; 4];
                r[0] = Some(next(n));
                r[1] = (next(4) != 0).then(|| next(3));
                r[2 + next(2) as usize] = Some(next(1000));
                r
            };
            let left: Vec<Binding> = (0..400).map(|_| row(25)).collect();
            let right: Vec<Binding> = (0..300).map(|_| row(25)).collect();
            let fx = join_sets_hash(&left, &right, &shared);
            let sip = join_sets_hash_with::<std::collections::hash_map::RandomState>(
                &left, &right, &shared,
            );
            let nested = join_sets_nested(&left, &right, &shared);
            assert!(!fx.is_empty());
            assert_eq!(fx, sip);
            assert_eq!(fx, nested);
        }
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

    /// ORDER BY may sort by a variable that is *not* in the projection; the
    /// sort must happen before projection so LIMIT keeps the right row.
    #[test]
    fn order_by_non_projected_var_with_limit() {
        let (d, s) = fixture();
        let q = parse("SELECT ?x WHERE { ?x <http://ex/salary> ?sal } ORDER BY DESC(?sal) LIMIT 1")
            .unwrap();
        let eval = Evaluator::new(&d, &s);
        let rs = match eval.evaluate(&q).unwrap() {
            QueryResult::Select(rs) => rs,
            other => panic!("expected SELECT, got {other:?}"),
        };
        assert_eq!(rs.row_count(), 1);
        // bob (3000) sorts above alice (2500) on ?sal, even though ?sal is not
        // selected — so the single kept row must be bob.
        assert!(rs.rows[0][0].as_ref().unwrap().contains("bob"));
    }

    /// ABS(i64::MIN) must widen instead of panicking (debug) / wrapping (release).
    #[test]
    fn abs_of_i64_min_widens_without_panic() {
        let mut d = Dictionary::new();
        let n = d.intern_term(&Term::iri("http://ex/n"));
        let p = d.intern_predicate(&Term::iri("http://ex/v").dict_key());
        let xsdint = "http://www.w3.org/2001/XMLSchema#integer";
        let min = d.intern_term(&Term::typed_literal("-9223372036854775808", xsdint));
        let mut s = TripleStore::new();
        s.bulk_load(vec![IdTriple::new(n, p, min)]);
        let q = parse("SELECT (ABS(?x) AS ?a) WHERE { ?s <http://ex/v> ?x }").unwrap();
        let rs = match Evaluator::new(&d, &s).evaluate(&q).unwrap() {
            QueryResult::Select(rs) => rs,
            other => panic!("expected SELECT, got {other:?}"),
        };
        assert_eq!(rs.row_count(), 1);
        assert!(rs.rows[0][0]
            .as_ref()
            .unwrap()
            .contains("9223372036854775808"));
    }

    /// Use a custom predicate function inside a FILTER (case-insensitive name).
    #[test]
    fn custom_function_in_filter() {
        let (d, s) = fixture();
        let q =
            parse("SELECT ?x WHERE { ?x <http://ex/salary> ?sal . FILTER(isHigh(?sal)) }").unwrap();
        let mut reg = FunctionRegistry::new();
        reg.register("isHigh", |args| {
            Some(Value::Bool(args.first()?.as_f64()? > 2800.0))
        });
        let rs = match Evaluator::new(&d, &s)
            .with_functions(reg)
            .evaluate(&q)
            .unwrap()
        {
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
        let q =
            parse("SELECT ?x (noSuchFn(?sal) AS ?d) WHERE { ?x <http://ex/salary> ?sal }").unwrap();
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
        let q =
            parse("SELECT (ABS(-5) AS ?r) WHERE { ?x <http://ex/salary> ?sal } LIMIT 1").unwrap();
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

/// Advanced graph functions inside the evaluator: CSR sourcing, cancellation
/// and the cast helpers (end-to-end behaviour: `tests/dt_gfun_e2e.rs`).
#[cfg(test)]
mod gfun_engine_tests {
    use super::*;
    use crate::model::IdTriple;
    use crate::parser::sparql::parse;
    use std::cell::Cell;
    use std::sync::atomic::AtomicBool;

    /// `a -p-> b -p-> c`.
    fn fixture() -> (Dictionary, TripleStore) {
        let mut d = Dictionary::new();
        let a = d.intern_term(&Term::iri("http://ex/a"));
        let b = d.intern_term(&Term::iri("http://ex/b"));
        let c = d.intern_term(&Term::iri("http://ex/c"));
        let p = d.intern_predicate(&Term::iri("http://ex/p").dict_key());
        let mut s = TripleStore::new();
        s.bulk_load(vec![IdTriple::new(a, p, b), IdTriple::new(b, p, c)]);
        (d, s)
    }

    fn cells(r: QueryResult) -> Vec<Vec<Option<String>>> {
        match r {
            QueryResult::Select(rs) => rs.rows,
            other => panic!("expected SELECT, got {other:?}"),
        }
    }

    const TWO_FNS: &str = "SELECT (kHopCount(<http://ex/a>, true, 2, {}) AS ?k) \
                           (bfsCount(?x, true, {}) AS ?b) WHERE { ?x <http://ex/p> ?y }";

    #[test]
    fn without_a_provider_the_evaluator_builds_its_own_csr() {
        let (d, s) = fixture();
        let q = parse("SELECT (SSSPLen(<http://ex/a>, true, {}) AS ?r) WHERE {}").unwrap();
        let rows = cells(Evaluator::new(&d, &s).evaluate(&q).unwrap());
        assert_eq!(rows.len(), 1);
        let cell = rows[0][0].clone().unwrap();
        let Ok(Term::Literal {
            value,
            datatype: None,
            lang: None,
        }) = parse_term(&cell)
        else {
            panic!("not a plain literal: {cell}")
        };
        assert_eq!(
            value,
            // Upstream's order (an `unordered_map` walk), see gfun::paths.
            r#"{"paths":[{"src":"<http://ex/a>","dst":"<http://ex/c>","length":2},{"src":"<http://ex/a>","dst":"<http://ex/b>","length":1},{"src":"<http://ex/a>","dst":"<http://ex/a>","length":0}]}"#
        );
    }

    #[test]
    fn the_provider_is_asked_once_per_query_and_only_when_needed() {
        let (d, s) = fixture();
        let calls = Cell::new(0);
        let shared = Arc::new(CsrGraph::build(&s).unwrap());
        let provider = || {
            calls.set(calls.get() + 1);
            Ok(Arc::clone(&shared))
        };
        let ev = Evaluator::new(&d, &s).with_csr_provider(&provider);
        // Two functions (the second over two rows) share one CSR.
        let rows = cells(ev.evaluate(&parse(TWO_FNS).unwrap()).unwrap());
        assert_eq!(rows.len(), 2);
        assert_eq!(calls.get(), 1);
        // No graph function, no CSR.
        let ev = Evaluator::new(&d, &s).with_csr_provider(&provider);
        ev.evaluate(&parse("SELECT ?x WHERE { ?x <http://ex/p> ?y }").unwrap())
            .unwrap();
        assert_eq!(calls.get(), 1);
        // A failing provider fails the query.
        let failing = || Err(GStoreError::Database("no CSR".into()));
        let ev = Evaluator::new(&d, &s).with_csr_provider(&failing);
        let e = ev.evaluate(&parse(TWO_FNS).unwrap()).unwrap_err();
        assert!(e.to_string().contains("no CSR"), "{e}");
    }

    #[test]
    fn graph_functions_run_under_the_evaluators_cancel_flag() {
        let (d, s) = fixture();
        let flag = Arc::new(AtomicBool::new(true));
        let ev = Evaluator::new(&d, &s).with_cancel(Arc::clone(&flag));
        let Query::Select(q) = parse(TWO_FNS).unwrap() else {
            unreachable!()
        };
        let items = graph_fn_items(&q).unwrap();
        let layout = VarLayout::build(&q.pattern);
        // `eval_graph_fn` (behind `eval_proj_call`) does not check the flag
        // itself: the library does, through the context the evaluator hands it.
        let r = ev.eval_proj_call(&items[0].call, &[], &layout, &HashSet::new(), true);
        assert!(matches!(r, Err(GStoreError::Cancelled)), "{r:?}");
        flag.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(ev
            .eval_proj_call(&items[0].call, &[], &layout, &HashSet::new(), true)
            .unwrap()
            .is_some());
    }

    #[test]
    fn upstream_projection_length_counts_unprojected_order_by_variables() {
        let len = |q: &str| match parse(q).unwrap() {
            Query::Select(s) => upstream_projection_len(&s),
            _ => unreachable!(),
        };
        let f = "(WCC({}) AS ?w)";
        assert_eq!(len(&format!("SELECT {f} WHERE {{ ?x ?p ?y }}")), 1);
        assert_eq!(
            len(&format!("SELECT {f} WHERE {{ ?x ?p ?y }} ORDER BY ?w")),
            1
        );
        assert_eq!(
            len(&format!(
                "SELECT {f} WHERE {{ ?x ?p ?y }} ORDER BY ?x DESC(?y)"
            )),
            3
        );
        assert_eq!(
            len(&format!(
                "SELECT ?x {f} WHERE {{ ?x ?p ?y }} ORDER BY (STR(?x))"
            )),
            2
        );
        assert!(pattern_is_trivially_empty(&GraphPattern::Empty));
        assert!(pattern_is_trivially_empty(&GraphPattern::Bgp(Vec::new())));
    }

    #[test]
    fn cast_helpers() {
        assert_eq!(parse_xsd_double("1e3"), Some(1000.0));
        assert_eq!(parse_xsd_double(" -.5E-1 "), Some(-0.05));
        assert_eq!(parse_xsd_double("+INF"), Some(f64::INFINITY));
        assert!(parse_xsd_double("NaN").unwrap().is_nan());
        for bad in [
            "inf", "Infinity", "nan", "1e", "e3", "1.2.3", "", "0x10", "1_000",
        ] {
            assert_eq!(parse_xsd_double(bad), None, "{bad}");
        }
        assert!(is_integer_lexical("+12"));
        assert!(!is_integer_lexical("-") && !is_integer_lexical("1.0"));
        assert!(is_decimal_lexical(".5") && is_decimal_lexical("5."));
        assert!(!is_decimal_lexical("."));
        assert_eq!(xsd_float_lexical(0.1), "0.1");
        assert_eq!(xsd_float_lexical(f64::NEG_INFINITY), "-INF");
        assert_eq!(xsd_float_lexical(1e40), "INF");
        assert_eq!(split_date_tz("2020-01-01"), ("2020-01-01", ""));
        assert_eq!(split_date_tz("-0044-03-15"), ("-0044-03-15", ""));
        assert_eq!(split_date_tz("2020-01-01Z"), ("2020-01-01", "Z"));
        assert_eq!(split_date_tz("2020-01-01-05:00"), ("2020-01-01", "-05:00"));
        assert_eq!(
            split_date_tz("2020-01-01T10:00:00"),
            ("2020-01-01T10:00:00", "")
        );
        assert_eq!(
            split_date_tz("2020-01-01T10:00:00.5+01:30"),
            ("2020-01-01T10:00:00.5", "+01:30")
        );
        assert_eq!(iri_local_name("http://ex/fn#double"), "double");
        assert_eq!(iri_local_name("http://ex/fn/double"), "double");
        assert_eq!(iri_local_name("urn:x:double"), "double");
        assert_eq!(
            XsdCast::from_iri(&format!("{XSD_NS}integer")),
            Some(XsdCast::Integer)
        );
        assert_eq!(XsdCast::from_iri(&format!("{XSD_NS}Integer")), None);
    }
}
