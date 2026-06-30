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
/// [ORDER BY …] [LIMIT/OFFSET]`.
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
}

impl SelectQuery {
    /// The output variable names, in column order.
    pub fn result_vars(&self) -> Vec<String> {
        match &self.projection {
            Projection::All => {
                let mut v = Vec::new();
                self.pattern.collect_vars(&mut v);
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
/// gStore models this as a `QueryTree`; here it's a small algebra. `OPTIONAL`
/// (left join) and `MINUS` are deferred (backlog item D); `UNION`, conjunction
/// (`Join`), and `FILTER` are supported.
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
