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
    /// `INSERT DATA { … }` — ground triples to add.
    InsertData(Vec<GroundTriple>),
    /// `DELETE DATA { … }` — ground triples to remove.
    DeleteData(Vec<GroundTriple>),
}

/// `SELECT [DISTINCT] (…|*) WHERE { … } [ORDER BY …] [LIMIT/OFFSET]`.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectQuery {
    pub distinct: bool,
    pub projection: Projection,
    pub pattern: GraphPattern,
    pub order_by: Vec<OrderCondition>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

/// `ASK { … }`.
#[derive(Debug, Clone, PartialEq)]
pub struct AskQuery {
    pub pattern: GraphPattern,
}

/// What a SELECT returns.
#[derive(Debug, Clone, PartialEq)]
pub enum Projection {
    /// `SELECT *` — every variable mentioned in the pattern.
    All,
    /// `SELECT ?a ?b` — the listed variables (without the leading `?`).
    Vars(Vec<String>),
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
    /// Conjunction of two patterns (join on shared variables).
    Join(Box<GraphPattern>, Box<GraphPattern>),
    /// Alternation: the union of both branches' solutions.
    Union(Box<GraphPattern>, Box<GraphPattern>),
    /// Constrain a pattern's solutions by FILTER expressions.
    Filter(Vec<Expr>, Box<GraphPattern>),
}

impl GraphPattern {
    /// Collect every triple pattern in textual order (for variable discovery).
    pub fn collect_triples<'a>(&'a self, out: &mut Vec<&'a TriplePattern>) {
        match self {
            GraphPattern::Empty => {}
            GraphPattern::Bgp(tps) => out.extend(tps.iter()),
            GraphPattern::Join(a, b) | GraphPattern::Union(a, b) => {
                a.collect_triples(out);
                b.collect_triples(out);
            }
            GraphPattern::Filter(_, inner) => inner.collect_triples(out),
        }
    }
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
#[derive(Debug, Clone, PartialEq)]
pub struct GroundTriple {
    pub subject: Term,
    pub predicate: Term,
    pub object: Term,
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
