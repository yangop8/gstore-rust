//! SPARQL abstract syntax tree (the subset the engine supports).
//!
//! Corresponds to gStore's `QueryTree` / `SPARQLquery`. Deferred SPARQL 1.1
//! features (OPTIONAL, UNION, aggregates, paths, sub-queries) are tracked in
//! `docs/REFACTOR_BACKLOG.md` item D.

use crate::model::Term;

/// XSD datatype IRIs used when materializing numeric / boolean literals.
pub mod xsd {
    pub const INTEGER: &str = "http://www.w3.org/2001/XMLSchema#integer";
    pub const DECIMAL: &str = "http://www.w3.org/2001/XMLSchema#decimal";
    pub const DOUBLE: &str = "http://www.w3.org/2001/XMLSchema#double";
    pub const BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";
    pub const STRING: &str = "http://www.w3.org/2001/XMLSchema#string";
}

/// rdf:type, the IRI that `a` abbreviates.
pub const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// The RDF collection vocabulary that `( … )` expands to.
pub mod rdf {
    pub const FIRST: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#first";
    pub const REST: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#rest";
    pub const NIL: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#nil";
}

/// Name prefix of the hidden variables that stand for the anonymous blank
/// nodes of a query (`[]`, `[ :p :o ]` and the cells of a collection
/// `( … )`). A SPARQL variable name cannot contain `:`, so a hidden variable
/// never collides with a variable the query names.
///
/// In a WHERE pattern a hidden variable matches like any other variable, but
/// it is not a query variable: `SELECT *` / `DESCRIBE *` leave it out. In a
/// CONSTRUCT or INSERT template, a hidden variable the WHERE clause does not
/// bind is instantiated as a fresh blank node per solution.
pub const ANON_VAR_PREFIX: &str = "_:";

/// Whether `name` is a hidden blank-node variable (see [`ANON_VAR_PREFIX`]).
pub fn is_anon_var(name: &str) -> bool {
    name.starts_with(ANON_VAR_PREFIX)
}

/// A fresh blank-node label, unique within this process and — through a
/// per-process random component — practically unique across processes, so a
/// minted node never merges with a blank node already in a store. Used for
/// the anonymous blank nodes of `INSERT DATA` and for template blank nodes.
pub fn fresh_bnode_label() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::OnceLock;
    static NONCE: OnceLock<u64> = OnceLock::new();
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nonce = *NONCE.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        // Mix in the pid so two processes started in the same instant differ.
        (nanos ^ (u64::from(std::process::id()) << 32)).wrapping_mul(0x9E37_79B9_7F4A_7C15)
    });
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("genid-{nonce:016x}-{n}")
}

/// An inline data table, `VALUES (?a ?b) { (…) (…) }`: the variables, then
/// one row per solution with a cell per variable (`None` = `UNDEF`).
pub type ValuesTable = (Vec<String>, Vec<Vec<Option<Term>>>);

/// A complete parsed request: a query or an update.
#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    Select(SelectQuery),
    Ask(AskQuery),
    Construct(ConstructQuery),
    Describe(DescribeQuery),
    /// A SPARQL UPDATE request: a `;`-separated sequence of operations.
    Update(Vec<UpdateOp>),
}

/// One SPARQL UPDATE operation.
#[derive(Debug, Clone, PartialEq)]
pub enum UpdateOp {
    /// `INSERT DATA { … }` — ground triples to add.
    InsertData(Vec<GroundTriple>),
    /// `DELETE DATA { … }` — ground triples to remove.
    DeleteData(Vec<GroundTriple>),
    /// `[DELETE { … }] [INSERT { … }] WHERE { … }` (and `DELETE WHERE { … }`,
    /// where `delete` and `pattern` are the same triples). Templates are
    /// instantiated per WHERE solution; deletes apply before inserts.
    Modify {
        delete: Vec<TriplePattern>,
        insert: Vec<TriplePattern>,
        pattern: GraphPattern,
    },
    /// `LOAD [SILENT] <iri>` — load an RDF document into the default graph.
    Load { source: String, silent: bool },
    /// `CLEAR [SILENT] (DEFAULT | NAMED | ALL | GRAPH <iri>)`.
    Clear { target: GraphTarget, silent: bool },
    /// `DROP [SILENT] (DEFAULT | NAMED | ALL | GRAPH <iri>)`.
    Drop { target: GraphTarget, silent: bool },
    /// `CREATE [SILENT] GRAPH <iri>`.
    Create { name: String, silent: bool },
    /// `ADD [SILENT] from TO to` (SPARQL 1.1 Update §3.2.5): insert every
    /// triple of graph `from` into graph `to`, keeping `to`'s data. A graph
    /// is `None` for `DEFAULT`, else the named graph's IRI.
    Add {
        from: Option<String>,
        to: Option<String>,
        silent: bool,
    },
    /// `MOVE [SILENT] from TO to` (§3.2.4): clear `to`, copy `from` into it,
    /// then drop `from`.
    Move {
        from: Option<String>,
        to: Option<String>,
        silent: bool,
    },
    /// `COPY [SILENT] from TO to` (§3.2.3): clear `to`, then copy `from` into
    /// it.
    Copy {
        from: Option<String>,
        to: Option<String>,
        silent: bool,
    },
}

/// The target of a `CLEAR`/`DROP` operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphTarget {
    Default,
    Named(String),
    All,
}

/// `DESCRIBE (* | (?var | <iri>)+ ) [WHERE { … }]`.
#[derive(Debug, Clone, PartialEq)]
pub struct DescribeQuery {
    /// Explicit targets: variables (resolved via the WHERE pattern) or terms.
    pub targets: Vec<PatternTerm>,
    /// `DESCRIBE *` — describe every variable bound by the WHERE pattern.
    pub all: bool,
    /// Optional WHERE pattern binding variable targets.
    pub pattern: Option<GraphPattern>,
}

/// `SELECT [DISTINCT] (…|*) WHERE { … } [GROUP BY …] [HAVING …]
/// [ORDER BY …] [LIMIT/OFFSET] [VALUES …]`.
///
/// A `GROUP BY (expr AS ?v)` condition is stored as `Expr::Var(v)` in
/// `group_by`, with `pattern` extended by `BIND(expr AS ?v)` (SPARQL 1.1
/// §18.2.4.1 evaluates it exactly so).
#[derive(Debug, Clone, PartialEq)]
pub struct SelectQuery {
    pub distinct: bool,
    pub projection: Projection,
    pub pattern: GraphPattern,
    pub group_by: Vec<Expr>,
    pub having: Vec<Expr>,
    pub order_by: Vec<OrderCondition>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// The trailing `VALUES` block (`ValuesClause` after the solution
    /// modifiers). It is joined with the solutions after grouping/HAVING and
    /// before the SELECT expressions, ORDER BY and projection (§18.2.4.3).
    pub values: Option<ValuesTable>,
}

impl SelectQuery {
    /// The output variable names, in column order. `SELECT *` lists the
    /// variables of the WHERE pattern, then those only the trailing `VALUES`
    /// binds; hidden blank-node variables ([`is_anon_var`]) are not listed.
    pub fn result_vars(&self) -> Vec<String> {
        match &self.projection {
            Projection::All => {
                let mut v = Vec::new();
                self.pattern.collect_vars(&mut v);
                if let Some((vars, _)) = &self.values {
                    for x in vars {
                        if !v.contains(x) {
                            v.push(x.clone());
                        }
                    }
                }
                v.retain(|x| !is_anon_var(x));
                v
            }
            Projection::Items(items) => items
                .iter()
                .map(|it| match it {
                    SelectItem::Var(v) => v.clone(),
                    SelectItem::Expr(_, v) => v.clone(),
                })
                .collect(),
        }
    }
}

/// `ASK { … }`.
#[derive(Debug, Clone, PartialEq)]
pub struct AskQuery {
    pub pattern: GraphPattern,
}

/// `CONSTRUCT { template } WHERE { … }`.
#[derive(Debug, Clone, PartialEq)]
pub struct ConstructQuery {
    /// The output-triple template (variables filled per solution).
    pub template: Vec<TriplePattern>,
    pub pattern: GraphPattern,
    pub order_by: Vec<OrderCondition>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

/// What a SELECT returns.
#[derive(Debug, Clone, PartialEq)]
pub enum Projection {
    /// `SELECT *` — every variable mentioned in the pattern.
    All,
    /// An explicit list of variables and/or `(expr AS ?v)` items.
    Items(Vec<SelectItem>),
}

/// One SELECT projection item.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    /// A bare variable `?v`.
    Var(String),
    /// A computed value `(expr AS ?v)` (may contain aggregates).
    Expr(Expr, String),
}

/// A graph-pattern algebra node — the evaluable shape of a WHERE clause.
///
/// gStore models this as a `QueryTree`; here it's a small algebra covering the
/// full SPARQL 1.1 shape: conjunction (`Join`), `UNION`, `OPTIONAL` (`LeftJoin`),
/// `MINUS`, `FILTER`, `BIND`/`Extend`, `VALUES`, sub-`SELECT`, `GRAPH`, `SERVICE`,
/// property paths, and `EXISTS`.
#[derive(Debug, Clone, PartialEq)]
pub enum GraphPattern {
    /// Matches the input unchanged (an empty group `{}`).
    Empty,
    /// A basic graph pattern: a conjunction of triple patterns.
    Bgp(Vec<TriplePattern>),
    /// A single property-path pattern: `subject path object`.
    Path(PathPattern),
    /// Conjunction of two patterns (join on shared variables).
    Join(Box<GraphPattern>, Box<GraphPattern>),
    /// Alternation: the union of both branches' solutions.
    Union(Box<GraphPattern>, Box<GraphPattern>),
    /// Constrain a pattern's solutions by FILTER expressions.
    Filter(Vec<Expr>, Box<GraphPattern>),
    /// `OPTIONAL`: left outer join. Trailing `Vec<Expr>` are FILTERs that live
    /// *inside* the OPTIONAL (evaluated as part of the join condition).
    LeftJoin(Box<GraphPattern>, Box<GraphPattern>, Vec<Expr>),
    /// `MINUS`: left solutions with no compatible right solution removed.
    Minus(Box<GraphPattern>, Box<GraphPattern>),
    /// `BIND(expr AS ?var)`: extend each solution of the inner pattern.
    Extend(Box<GraphPattern>, String, Expr),
    /// `VALUES`: an inline table of solutions (vars + rows; `None` = UNDEF).
    Values(Vec<String>, Vec<Vec<Option<Term>>>),
    /// A nested `SELECT` sub-query.
    SubSelect(Box<SelectQuery>),
    /// `GRAPH (<iri> | ?g) { … }`: evaluate the inner pattern against a named
    /// graph (or, for a variable, against each named graph, binding it).
    Graph(GraphTerm, Box<GraphPattern>),
    /// `SERVICE [SILENT] (<iri> | ?svc) { … }`: SPARQL 1.1 federated query — the
    /// inner pattern is shipped to a remote SPARQL endpoint and its returned
    /// solutions are joined with the outer ones. `silent` swallows
    /// connection/parse failures, yielding the join identity (so outer rows are
    /// preserved).
    ///
    /// Scope limitations (see `docs/REFACTOR_BACKLOG.md`): the inner pattern
    /// serialized to the remote is the BGP/UNION subset (FILTER/OPTIONAL/BIND/
    /// paths/sub-SELECT are not shipped); a constant `<iri>` endpoint is the
    /// supported case; and because query evaluation is infallible, a *non*-SILENT
    /// remote failure or an unsupported inner shape currently yields no rows
    /// rather than raising an error. The whole inner relation is fetched and
    /// joined locally (no bound-join pushdown).
    Service {
        endpoint: ServiceRef,
        silent: bool,
        pattern: Box<GraphPattern>,
    },
}

/// The graph reference of a `GRAPH` pattern: a constant IRI or a variable.
#[derive(Debug, Clone, PartialEq)]
pub enum GraphTerm {
    Var(String),
    Iri(String),
}

/// The endpoint reference of a `SERVICE` pattern: a constant IRI or a variable.
#[derive(Debug, Clone, PartialEq)]
pub enum ServiceRef {
    /// A constant endpoint IRI, e.g. `<http://dbpedia.org/sparql>`.
    Iri(String),
    /// A variable endpoint `?svc` (uncommon; resolved per outer solution).
    Var(String),
}

impl GraphPattern {
    /// Collect every plain triple pattern in textual order (for variable
    /// discovery). Path/Values/sub-select variables are gathered separately by
    /// the engine; this drives BGP variable layout.
    pub fn collect_triples<'a>(&'a self, out: &mut Vec<&'a TriplePattern>) {
        match self {
            GraphPattern::Empty
            | GraphPattern::Path(_)
            | GraphPattern::Values(_, _)
            | GraphPattern::SubSelect(_) => {}
            GraphPattern::Bgp(tps) => out.extend(tps.iter()),
            GraphPattern::Join(a, b) | GraphPattern::Union(a, b) | GraphPattern::Minus(a, b) => {
                a.collect_triples(out);
                b.collect_triples(out);
            }
            GraphPattern::LeftJoin(a, b, _) => {
                a.collect_triples(out);
                b.collect_triples(out);
            }
            GraphPattern::Filter(_, inner) | GraphPattern::Extend(inner, _, _) => {
                inner.collect_triples(out)
            }
            GraphPattern::Graph(_, inner) => inner.collect_triples(out),
            GraphPattern::Service { pattern, .. } => pattern.collect_triples(out),
        }
    }

    /// Collect every variable name the pattern can bind (in appearance order).
    pub fn collect_vars(&self, out: &mut Vec<String>) {
        let push = |v: &str, out: &mut Vec<String>| {
            if !out.iter().any(|x| x == v) {
                out.push(v.to_string());
            }
        };
        match self {
            GraphPattern::Empty => {}
            GraphPattern::Bgp(tps) => {
                for tp in tps {
                    for pos in [&tp.subject, &tp.predicate, &tp.object] {
                        if let PatternTerm::Var(v) = pos {
                            push(v, out);
                        }
                    }
                }
            }
            GraphPattern::Path(p) => {
                if let PatternTerm::Var(v) = &p.subject {
                    push(v, out);
                }
                if let PatternTerm::Var(v) = &p.object {
                    push(v, out);
                }
            }
            GraphPattern::Join(a, b) | GraphPattern::Union(a, b) | GraphPattern::Minus(a, b) => {
                a.collect_vars(out);
                b.collect_vars(out);
            }
            GraphPattern::LeftJoin(a, b, _) => {
                a.collect_vars(out);
                b.collect_vars(out);
            }
            GraphPattern::Filter(_, inner) => inner.collect_vars(out),
            GraphPattern::Extend(inner, v, _) => {
                inner.collect_vars(out);
                push(v, out);
            }
            GraphPattern::Values(vars, _) => {
                for v in vars {
                    push(v, out);
                }
            }
            GraphPattern::SubSelect(sq) => {
                for v in sq.result_vars() {
                    push(&v, out);
                }
            }
            GraphPattern::Graph(g, inner) => {
                if let GraphTerm::Var(v) = g {
                    push(v, out);
                }
                inner.collect_vars(out);
            }
            GraphPattern::Service {
                endpoint, pattern, ..
            } => {
                if let ServiceRef::Var(v) = endpoint {
                    push(v, out);
                }
                pattern.collect_vars(out);
            }
        }
    }
}

/// A property-path pattern: `subject <path> object`.
#[derive(Debug, Clone, PartialEq)]
pub struct PathPattern {
    pub subject: PatternTerm,
    pub path: PathExpr,
    pub object: PatternTerm,
}

/// A SPARQL property path expression.
#[derive(Debug, Clone, PartialEq)]
pub enum PathExpr {
    /// A single predicate IRI.
    Pred(String),
    /// Inverse path `^p`.
    Inverse(Box<PathExpr>),
    /// Sequence `p1 / p2`.
    Seq(Box<PathExpr>, Box<PathExpr>),
    /// Alternative `p1 | p2`.
    Alt(Box<PathExpr>, Box<PathExpr>),
    /// Zero-or-more `p*`.
    ZeroOrMore(Box<PathExpr>),
    /// One-or-more `p+`.
    OneOrMore(Box<PathExpr>),
    /// Zero-or-one `p?`.
    ZeroOrOne(Box<PathExpr>),
    /// Negated property set `!(p1|…)` (only simple/inverse preds inside).
    NegatedSet(Vec<(String, bool)>), // (predicate IRI, is_inverse)
}

/// A triple pattern: each position is a variable or a concrete term.
#[derive(Debug, Clone, PartialEq)]
pub struct TriplePattern {
    pub subject: PatternTerm,
    pub predicate: PatternTerm,
    pub object: PatternTerm,
}

/// One position of a triple pattern.
#[derive(Debug, Clone, PartialEq)]
pub enum PatternTerm {
    /// A query variable, stored without the `?`/`$` sigil.
    Var(String),
    /// A concrete, already prefix-expanded RDF term.
    Term(Term),
}

impl PatternTerm {
    pub fn as_var(&self) -> Option<&str> {
        match self {
            PatternTerm::Var(v) => Some(v),
            _ => None,
        }
    }
}

/// A ground triple (used by INSERT/DELETE DATA): all positions concrete.
/// `graph` names the target named graph (`None` = the default graph).
#[derive(Debug, Clone, PartialEq)]
pub struct GroundTriple {
    pub subject: Term,
    pub predicate: Term,
    pub object: Term,
    pub graph: Option<String>,
}

/// One `ORDER BY` key.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderCondition {
    pub expr: Expr,
    pub descending: bool,
}

/// A FILTER / ORDER BY expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A variable reference.
    Var(String),
    /// A constant RDF term (IRI or literal).
    Const(Term),
    Or(Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    /// Unary minus / plus.
    Unary(UnaryOp, Box<Expr>),
    Compare(CompareOp, Box<Expr>, Box<Expr>),
    /// `expr IN (e1, …)` / `expr NOT IN (e1, …)` (SPARQL 1.1 §17.4.1.9-10;
    /// `negated` for NOT IN). `list` may be empty: `x IN ()` is false.
    In {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    Arith(ArithOp, Box<Expr>, Box<Expr>),
    /// A builtin function call, e.g. `ABS`, `STR`, `REGEX`, `BOUND`.
    Builtin(String, Vec<Expr>),
    /// `EXISTS { … }` / `NOT EXISTS { … }` — true iff the pattern has a solution
    /// compatible with the current binding. The `bool` is `true` for NOT EXISTS.
    Exists(bool, Box<GraphPattern>),
    /// An aggregate over a group, e.g. `COUNT(DISTINCT ?x)`, `SUM(?v)`.
    Aggregate {
        func: AggFunc,
        distinct: bool,
        /// `None` for `COUNT(*)`.
        arg: Option<Box<Expr>>,
        /// Separator for `GROUP_CONCAT`.
        sep: Option<String>,
    },
    /// An upstream gStore advanced graph function (`builtInCall` alternatives
    /// such as `shortestPath(?x, <b>, true, {<p>})`). `args` follows the
    /// grammar alternative's positional shape exactly — one [`GArg`] per
    /// grammar slot in source order (see [`GFnName::params`]); an optional
    /// slot that was omitted contributes no entry.
    ///
    /// Upstream accepts every one of them as the whole expression of a SELECT
    /// projection `(f(…) AS ?v)`; only [`GFnName::filter_callable`] ones may
    /// also appear nested inside an expression (FILTER, HAVING, BIND, …).
    GraphFn {
        func: GFnName,
        args: Vec<GArg>,
    },
    /// An upstream `PFN.name({…})` personalised-function call (also accepted
    /// without the dot, `PFN name({…})`, as the upstream grammar spells it).
    /// `json` is the `jsonVars` argument exactly as upstream hands it to the
    /// function: the raw lexemes of its tokens concatenated without the
    /// whitespace between them, e.g. `{"k":["<a>","<b>"]}`.
    Pfn {
        name: String,
        json: String,
    },
    /// A function call through an IRI, `<iri>(args…)` or `pfx:name(args…)`
    /// (SPARQL `iriOrFunction`). `iri` is fully expanded. Calls into the
    /// predeclared `rs:` namespace (`urn:gstore-rust:fn:`) name the legacy
    /// gstore-rust graph built-ins, e.g. `rs:SSSP(<a>, <b>)`.
    IriCall {
        iri: String,
        args: Vec<Expr>,
    },
}

/// A `varOrIri` argument of an advanced graph function.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum VarOrIri {
    /// A query variable (name without the `?`/`$` sigil).
    Var(String),
    /// A fully expanded IRI (prefixes and BASE resolved, no angle brackets).
    Iri(String),
}

/// The value of a named advanced-function argument (`alpha=`, `maxIter=`,
/// `tol=`, `increase=`). The variant follows the grammar slot, not how the
/// literal was spelled: `maxIter` is a `num_integer` ([`GNum::Int`]); `alpha`,
/// `tol` and `increase` are `numericLiteral`s ([`GNum::Num`], so `alpha=1`
/// gives `Num(1.0)`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GNum {
    Int(i64),
    Num(f64),
}

/// One argument of an [`Expr::GraphFn`], in grammar-slot order.
#[derive(Debug, Clone, PartialEq)]
pub enum GArg {
    /// `varOrIri`.
    Node(VarOrIri),
    /// `booleanLiteral` (the `directed` flag).
    Bool(bool),
    /// `integerLiteral` (signed: `k`, `retNum`, `hopCnt`, …) or `num_integer`
    /// (unsigned: `maximumKplex`'s `k`, `coreTruss`'s two parameters). Always
    /// within the C `int` range: upstream reads these with `stoi`, so a
    /// larger literal is a parse error.
    Int(i64),
    /// `numericLiteral` (`confidenceDegree`'s six weights), always as `f64`.
    Num(f64),
    /// `predSet`: `{}` (no restriction), `{<p>, …}` (allowed predicates) and
    /// `!<p>` negations (excluded predicates), as expanded IRIs in source
    /// order. Upstream semantics of the combination are the evaluator's job.
    PredSet {
        include: Vec<String>,
        exclude: Vec<String>,
    },
    /// `KEYWORD '=' value`; `key` is the canonical upstream spelling
    /// (`alpha`, `maxIter`, `tol`, `increase`) whatever case the query used.
    Named { key: String, value: GNum },
}

/// One grammar slot of an advanced graph function's argument list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GSlot {
    /// `varOrIri` → [`GArg::Node`].
    Node,
    /// `booleanLiteral` → [`GArg::Bool`].
    Bool,
    /// `integerLiteral` (optionally signed) → [`GArg::Int`].
    Int,
    /// `num_integer` (unsigned) → [`GArg::Int`].
    UInt,
    /// `numericLiteral` → [`GArg::Num`].
    Num,
    /// `predSet` → [`GArg::PredSet`].
    PredSet,
    /// `KEYWORD '=' numericLiteral` → [`GArg::Named`] with [`GNum::Num`].
    NamedNum(&'static str),
    /// `KEYWORD '=' num_integer` → [`GArg::Named`] with [`GNum::Int`].
    NamedUInt(&'static str),
}

impl GSlot {
    /// The grammar's name for the slot, for signatures and error messages.
    pub fn describe(self) -> String {
        match self {
            GSlot::Node => "varOrIri".into(),
            GSlot::Bool => "booleanLiteral".into(),
            GSlot::Int => "integerLiteral".into(),
            GSlot::UInt => "num_integer".into(),
            GSlot::Num => "numericLiteral".into(),
            GSlot::PredSet => "predSet".into(),
            GSlot::NamedNum(k) => format!("{k}=numericLiteral"),
            GSlot::NamedUInt(k) => format!("{k}=num_integer"),
        }
    }
}

/// A grammar slot plus whether it may be omitted (`( … )?` in the grammar).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GParam {
    pub slot: GSlot,
    pub optional: bool,
}

const fn req(slot: GSlot) -> GParam {
    GParam {
        slot,
        optional: false,
    }
}

const fn opt(slot: GSlot) -> GParam {
    GParam {
        slot,
        optional: true,
    }
}

use GSlot::{Bool as SB, Int as SI, Node as SN, Num as SNum, PredSet as SP, UInt as SU};

const P_UV_DIR_PRED: &[GParam] = &[req(SN), req(SN), req(SB), req(SP)];
const P_UV_DIR_K_PRED: &[GParam] = &[req(SN), req(SN), req(SB), req(SI), req(SP)];
const P_UV_DIR_K_PRED_RET: &[GParam] = &[req(SN), req(SN), req(SB), req(SI), req(SP), opt(SI)];
const P_U_DIR_PRED: &[GParam] = &[req(SN), req(SB), req(SP)];
const P_U_DIR_K_PRED: &[GParam] = &[req(SN), req(SB), req(SI), req(SP)];
const P_U_DIR_K_PRED_RET: &[GParam] = &[req(SN), req(SB), req(SI), req(SP), opt(SI)];
const P_DIR_PRED: &[GParam] = &[req(SB), req(SP)];
const P_PRED: &[GParam] = &[req(SP)];
const P_PPR: &[GParam] = &[req(SN), req(SI), req(SP), req(SI)];
const P_PR: &[GParam] = &[
    req(SB),
    req(SP),
    req(SI),
    req(GSlot::NamedNum("alpha")),
    req(GSlot::NamedUInt("maxIter")),
    req(GSlot::NamedNum("tol")),
];
const P_LABELPROP: &[GParam] = &[req(SB), req(SP), req(GSlot::NamedUInt("maxIter"))];
const P_CLUSTERCOEFF: &[GParam] = &[opt(SN), req(SB), req(SP)];
const P_MAXIMUMKPLEX: &[GParam] = &[req(SP), opt(SU)];
const P_CORETRUSS: &[GParam] = &[req(SP), req(SU), req(SU)];
const P_LOUVAIN: &[GParam] = &[
    req(SB),
    req(SP),
    req(GSlot::NamedUInt("maxIter")),
    req(GSlot::NamedNum("increase")),
];
const P_CONFIDENCEDEGREE: &[GParam] = &[
    req(SNum),
    req(SNum),
    req(SNum),
    req(SNum),
    req(SNum),
    req(SNum),
];
const P_JACCARD: &[GParam] = &[req(SN), req(SP), req(SI), req(SI)];
const P_DEGREECORRELATION: &[GParam] = &[req(SN), req(SI), req(SP)];

/// An upstream gStore 1.4 advanced graph function: every path/analytics
/// alternative of the `builtInCall` grammar rule (`SPARQL.g4`) except
/// `K_PFN`, which is [`Expr::Pfn`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GFnName {
    SimpleCyclePath,
    SimpleCycleBoolean,
    CyclePath,
    CycleBoolean,
    ShortestPath,
    ShortestPathLen,
    KHopReachable,
    KHopEnumerate,
    KHopReachablePath,
    Ppr,
    TriangleCounting,
    ClosenessCentrality,
    BfsCount,
    Pr,
    Sssp,
    SsspLen,
    LabelProp,
    Wcc,
    ClusterCoeff,
    MaximumKplex,
    CoreTruss,
    KHopCount,
    KHopNeighbor,
    ShortestPathCount,
    Louvain,
    ConfidenceDegree,
    DiameterEstimation,
    BetweennessCentrality,
    JaccardSimilarity,
    DegreeCorrelation,
    ShortestPaths,
    KHopAllNeighbors,
}

impl GFnName {
    /// Every advanced function, in grammar order.
    pub const ALL: [GFnName; 32] = [
        GFnName::SimpleCyclePath,
        GFnName::SimpleCycleBoolean,
        GFnName::CyclePath,
        GFnName::CycleBoolean,
        GFnName::ShortestPath,
        GFnName::ShortestPathLen,
        GFnName::KHopReachable,
        GFnName::KHopEnumerate,
        GFnName::KHopReachablePath,
        GFnName::Ppr,
        GFnName::TriangleCounting,
        GFnName::ClosenessCentrality,
        GFnName::BfsCount,
        GFnName::Pr,
        GFnName::Sssp,
        GFnName::SsspLen,
        GFnName::LabelProp,
        GFnName::Wcc,
        GFnName::ClusterCoeff,
        GFnName::MaximumKplex,
        GFnName::CoreTruss,
        GFnName::KHopCount,
        GFnName::KHopNeighbor,
        GFnName::ShortestPathCount,
        GFnName::Louvain,
        GFnName::ConfidenceDegree,
        GFnName::DiameterEstimation,
        GFnName::BetweennessCentrality,
        GFnName::JaccardSimilarity,
        GFnName::DegreeCorrelation,
        GFnName::ShortestPaths,
        GFnName::KHopAllNeighbors,
    ];

    /// The upstream lexer keyword (upper case; matched case-insensitively).
    pub fn keyword(self) -> &'static str {
        match self {
            GFnName::SimpleCyclePath => "SIMPLECYCLEPATH",
            GFnName::SimpleCycleBoolean => "SIMPLECYCLEBOOLEAN",
            GFnName::CyclePath => "CYCLEPATH",
            GFnName::CycleBoolean => "CYCLEBOOLEAN",
            GFnName::ShortestPath => "SHORTESTPATH",
            GFnName::ShortestPathLen => "SHORTESTPATHLEN",
            GFnName::KHopReachable => "KHOPREACHABLE",
            GFnName::KHopEnumerate => "KHOPENUMERATE",
            GFnName::KHopReachablePath => "KHOPREACHABLEPATH",
            GFnName::Ppr => "PPR",
            GFnName::TriangleCounting => "TRIANGLECOUNTING",
            GFnName::ClosenessCentrality => "CLOSENESSCENTRALITY",
            GFnName::BfsCount => "BFSCOUNT",
            GFnName::Pr => "PR",
            GFnName::Sssp => "SSSP",
            GFnName::SsspLen => "SSSPLEN",
            GFnName::LabelProp => "LABELPROP",
            GFnName::Wcc => "WCC",
            GFnName::ClusterCoeff => "CLUSTERCOEFF",
            GFnName::MaximumKplex => "MAXIMUMKPLEX",
            GFnName::CoreTruss => "CORETRUSS",
            GFnName::KHopCount => "KHOPCOUNT",
            GFnName::KHopNeighbor => "KHOPNEIGHBOR",
            GFnName::ShortestPathCount => "SHORTESTPATHCOUNT",
            GFnName::Louvain => "LOUVAIN",
            GFnName::ConfidenceDegree => "CONFIDENCEDEGREE",
            GFnName::DiameterEstimation => "DIAMETERESTIMATION",
            GFnName::BetweennessCentrality => "BETWEENNESSCENTRALITY",
            GFnName::JaccardSimilarity => "JACCARDSIMILARITY",
            GFnName::DegreeCorrelation => "DEGREECORRELATION",
            GFnName::ShortestPaths => "SHORTESTPATHS",
            GFnName::KHopAllNeighbors => "KHOPALLNEIGHBORS",
        }
    }

    /// The function's name as upstream documents it (`SPARQL_QUERY.md`), for
    /// messages and signatures.
    pub fn name(self) -> &'static str {
        match self {
            GFnName::SimpleCyclePath => "simpleCyclePath",
            GFnName::SimpleCycleBoolean => "simpleCycleBoolean",
            GFnName::CyclePath => "cyclePath",
            GFnName::CycleBoolean => "cycleBoolean",
            GFnName::ShortestPath => "shortestPath",
            GFnName::ShortestPathLen => "shortestPathLen",
            GFnName::KHopReachable => "kHopReachable",
            GFnName::KHopEnumerate => "kHopEnumerate",
            GFnName::KHopReachablePath => "kHopReachablePath",
            GFnName::Ppr => "PPR",
            GFnName::TriangleCounting => "triangleCounting",
            GFnName::ClosenessCentrality => "closenessCentrality",
            GFnName::BfsCount => "bfsCount",
            GFnName::Pr => "PR",
            GFnName::Sssp => "SSSP",
            GFnName::SsspLen => "SSSPLen",
            GFnName::LabelProp => "labelProp",
            GFnName::Wcc => "WCC",
            GFnName::ClusterCoeff => "clusterCoeff",
            GFnName::MaximumKplex => "maximumKplex",
            GFnName::CoreTruss => "coreTruss",
            GFnName::KHopCount => "kHopCount",
            GFnName::KHopNeighbor => "kHopNeighbor",
            GFnName::ShortestPathCount => "shortestPathCount",
            GFnName::Louvain => "louvain",
            GFnName::ConfidenceDegree => "confidenceDegree",
            GFnName::DiameterEstimation => "diameterEstimation",
            GFnName::BetweennessCentrality => "betweennessCentrality",
            GFnName::JaccardSimilarity => "JaccardSimilarity",
            GFnName::DegreeCorrelation => "degreeCorrelation",
            GFnName::ShortestPaths => "shortestPaths",
            GFnName::KHopAllNeighbors => "kHopAllNeighbors",
        }
    }

    /// The function whose keyword is `word` (any case), if any.
    pub fn from_keyword(word: &str) -> Option<GFnName> {
        GFnName::ALL
            .iter()
            .copied()
            .find(|f| f.keyword().eq_ignore_ascii_case(word))
    }

    /// The argument shape of the grammar alternative, slot by slot:
    ///
    /// | function | shape |
    /// |---|---|
    /// | simpleCyclePath, simpleCycleBoolean, cyclePath, cycleBoolean, shortestPath, shortestPathLen, shortestPathCount, shortestPaths | `(u, v, directed, predSet)` |
    /// | kHopReachable, kHopReachablePath | `(u, v, directed, k, predSet)` |
    /// | kHopEnumerate | `(u, v, directed, k, predSet [, retNum])` |
    /// | PPR | `(u, hopCnt, predSet, retNum)` |
    /// | triangleCounting, labelProp¹, louvain¹ | `(directed, predSet …)` |
    /// | closenessCentrality, bfsCount, SSSP, SSSPLen, betweennessCentrality | `(u, directed, predSet)` |
    /// | PR | `(directed, predSet, retNum, alpha=num, maxIter=int, tol=num)` |
    /// | WCC, diameterEstimation | `(predSet)` |
    /// | clusterCoeff | `([u,] directed, predSet)` |
    /// | maximumKplex | `(predSet [, k])` (`k` unsigned) |
    /// | coreTruss | `(predSet, int, int)` (unsigned) |
    /// | kHopCount | `(u, directed, k, predSet)` |
    /// | kHopNeighbor, kHopAllNeighbors | `(u, directed, k, predSet [, retNum])` |
    /// | confidenceDegree | six `numericLiteral`s |
    /// | JaccardSimilarity | `(u, predSet, k, retNum)` |
    /// | degreeCorrelation | `(u, k, predSet)` |
    ///
    /// ¹ `labelProp(directed, predSet, maxIter=int)`,
    /// `louvain(directed, predSet, maxIter=int, increase=num)`.
    ///
    /// The grammar is authoritative where the upstream docs differ (the docs
    /// omit PR's `retNum` and spell `PR`/`labelProp`/`louvain`'s trailing
    /// parameters positionally; the grammar requires the `key=` forms).
    pub fn params(self) -> &'static [GParam] {
        use GFnName as F;
        match self {
            F::SimpleCyclePath
            | F::SimpleCycleBoolean
            | F::CyclePath
            | F::CycleBoolean
            | F::ShortestPath
            | F::ShortestPathLen
            | F::ShortestPathCount
            | F::ShortestPaths => P_UV_DIR_PRED,
            F::KHopReachable | F::KHopReachablePath => P_UV_DIR_K_PRED,
            F::KHopEnumerate => P_UV_DIR_K_PRED_RET,
            F::Ppr => P_PPR,
            F::TriangleCounting => P_DIR_PRED,
            F::ClosenessCentrality
            | F::BfsCount
            | F::Sssp
            | F::SsspLen
            | F::BetweennessCentrality => P_U_DIR_PRED,
            F::Pr => P_PR,
            F::LabelProp => P_LABELPROP,
            F::Wcc | F::DiameterEstimation => P_PRED,
            F::ClusterCoeff => P_CLUSTERCOEFF,
            F::MaximumKplex => P_MAXIMUMKPLEX,
            F::CoreTruss => P_CORETRUSS,
            F::KHopCount => P_U_DIR_K_PRED,
            F::KHopNeighbor | F::KHopAllNeighbors => P_U_DIR_K_PRED_RET,
            F::Louvain => P_LOUVAIN,
            F::ConfidenceDegree => P_CONFIDENCEDEGREE,
            F::JaccardSimilarity => P_JACCARD,
            F::DegreeCorrelation => P_DEGREECORRELATION,
        }
    }

    /// The grammar signature, e.g.
    /// `kHopEnumerate(varOrIri, varOrIri, booleanLiteral, integerLiteral, predSet [, integerLiteral])`.
    pub fn signature(self) -> String {
        let mut s = format!("{}(", self.name());
        for (i, p) in self.params().iter().enumerate() {
            let slot = p.slot.describe();
            match (p.optional, i) {
                // A leading optional slot carries its own trailing comma.
                (true, 0) => s.push_str(&format!("[{slot}, ] ")),
                (true, _) => s.push_str(&format!(" [, {slot}]")),
                (false, 0) => s.push_str(&slot),
                (false, _) if self.params()[i - 1].optional && i == 1 => s.push_str(&slot),
                (false, _) => s.push_str(&format!(", {slot}")),
            }
        }
        s.push(')');
        s
    }

    /// Whether upstream also accepts the call nested inside an expression
    /// (its FILTER/HAVING/BIND comp-tree builder, `QueryParser.cpp`
    /// `buildCompTree`): only `simpleCycleBoolean`, `cycleBoolean`,
    /// `shortestPathLen` and `kHopReachable`. All others are accepted only as
    /// the whole expression of a SELECT projection `(f(…) AS ?v)`.
    pub fn filter_callable(self) -> bool {
        matches!(
            self,
            GFnName::SimpleCycleBoolean
                | GFnName::CycleBoolean
                | GFnName::ShortestPathLen
                | GFnName::KHopReachable
        )
    }
}

impl std::fmt::Display for GFnName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Aggregate functions (SPARQL 1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    Sample,
    GroupConcat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Plus,
}
